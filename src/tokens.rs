use crate::transcript::{assistant_section_messages, tail_user_text_capped, ParsedFile};
use crate::types::{AppConfig, ChatMessage, Role};

async fn request_token_count(
  api_base: &str,
  client: &reqwest::Client,
  model: &str,
  messages: &[ChatMessage],
) -> Result<usize, String> {
  let mut content = String::new();
  for m in messages {
    if let Some(c) = &m.content {
      content.push_str(c);
    }
    if let Some(r) = &m.reasoning_content {
      content.push('\n');
      content.push_str(r);
    }
    if let Some(calls) = &m.tool_calls {
      for c in calls {
        content.push('\n');
        content.push_str(&c.arguments);
      }
    }
    content.push('\n');
  }
  if content.is_empty() {
    return Ok(0);
  }
  let mut last_err = String::new();
  for (url, body) in [
    (format!("{api_base}/tokenize"), serde_json::json!({"content": content.clone(), "model": model})),
    (format!("{api_base}/tokenize"), serde_json::json!({"content": content.clone()})),
    (format!("{api_base}/v1/tokenize"), serde_json::json!({"content": content.clone(), "model": model})),
    (format!("{api_base}/v1/tokenize"), serde_json::json!({"content": content.clone()})),
  ] {
    let resp = match client.post(&url).json(&body).send().await {
      Ok(r) => r,
      Err(e) => {
        last_err = format!("{url}: {e}");
        continue;
      }
    };
    if !resp.status().is_success() {
      last_err = format!("{url}: HTTP {}", resp.status());
      continue;
    }
    if let Ok(v) = resp.json::<serde_json::Value>().await
      && let Some(toks) = v.get("tokens").and_then(|t| t.as_array())
    {
      return Ok(toks.len());
    }
    last_err = format!("{url}: no `tokens` array in response");
  }
  Err(format!("cannot reach model tokenizer: {last_err}"))
}

async fn request_token_counts(
  api_base: &str,
  client: &reqwest::Client,
  model: &str,
  messages: &[ChatMessage],
) -> Result<Vec<usize>, String> {
  let futs = messages
    .iter()
    .map(|m| request_token_count(api_base, client, model, std::slice::from_ref(m)));
  futures_util::future::join_all(futs)
    .await
    .into_iter()
    .collect()
}

#[derive(Debug)]
pub struct RequestPlan {
  pub messages: Vec<ChatMessage>,
  pub dropped: usize,
  pub token_count: usize,
}

pub async fn build_request_messages(
  text: &str,
  pf: &ParsedFile,
  cfg: &AppConfig,
  http: &reqwest::Client,
) -> Result<RequestPlan, String> {
  let mut messages = pf.history.clone();
  let cap = pf.dangling.as_ref().map(|d| d.resume_offset);
  let section = match cap {
    Some(cap) => tail_user_text_capped(text, cap),
    None => crate::transcript::tail_user_text(text),
  };
  if let Some(section) = section {
    messages.extend(crate::transcript::tail_sections(&section));
  }
  if let Some(d) = &pf.dangling
    && !d.partial_text.trim().is_empty()
  {
    messages.extend(assistant_section_messages(&d.partial_text));
  }
  let context_budget = context_budget(pf, cfg);
  if messages.is_empty() {
    return Ok(RequestPlan { messages, dropped: 0, token_count: 0 });
  }
  let counts = request_token_counts(
    &cfg.api_base,
    http,
    pf.config.model.as_deref().unwrap_or(&cfg.model),
    &messages,
  )
  .await?;
  let (truncated, kept_sum) = truncate_messages(messages, &counts, context_budget);
  if kept_sum > context_budget as usize {
    return Err(format!(
      "request exceeds the context budget even after dropping old turns: {kept_sum} > {context_budget} tokens; trim the transcript"
    ));
  }
  Ok(RequestPlan {
    dropped: counts.len() - truncated.len(),
    token_count: kept_sum,
    messages: truncated,
  })
}

pub fn context_budget(pf: &ParsedFile, cfg: &AppConfig) -> u32 {
  pf.config.max_context_tokens.unwrap_or(cfg.max_context_tokens)
}

#[cfg(test)]
pub(crate) fn msg_chars(m: &ChatMessage) -> usize {
  m.content.as_ref().map_or(0, |c| c.len())
    + m.reasoning_content.as_ref().map_or(0, |c| c.len())
    + m
      .tool_calls
      .as_ref()
      .map_or(0, |t| t.iter().map(|c| c.arguments.len() + 16).sum())
    + m.name.as_ref().map_or(0, |n| n.len())
}

pub fn truncate_messages(
  messages: Vec<ChatMessage>,
  token_counts: &[usize],
  max_tokens: u32,
) -> (Vec<ChatMessage>, usize) {
  if messages.is_empty() {
    return (messages, 0);
  }
  let budget = max_tokens as usize;
  let total: usize = token_counts.iter().sum();
  if total <= budget || messages.len() <= 2 {
    return (messages, total);
  }
  let sys_tokens = token_counts[0];
  let mut keep_from = messages.len() - 1;
  let mut sum = sys_tokens + token_counts[1..].iter().sum::<usize>();
  for (i, &toks) in token_counts.iter().enumerate().skip(1) {
    sum -= toks;
    if sum <= budget {
      keep_from = (i + 1).min(messages.len() - 1);
      break;
    }
  }
  while keep_from + 1 < messages.len() && !is_turn_start(&messages[keep_from]) {
    keep_from += 1;
  }
  if keep_from + 1 == messages.len() && !is_turn_start(&messages[keep_from]) {
    while keep_from > 0 && !is_turn_start(&messages[keep_from]) {
      keep_from -= 1;
    }
  }
  if keep_from > 1 {
    let kept_sum = sys_tokens + token_counts[keep_from..].iter().sum::<usize>();
    let mut out = Vec::with_capacity(messages.len() - keep_from + 1);
    out.push(messages[0].clone());
    out.extend(messages[keep_from..].iter().cloned());
    return (out, kept_sum);
  }
  (messages, total)
}

fn is_turn_start(m: &ChatMessage) -> bool {
  matches!(m.role, Role::User | Role::Assistant)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::types::ToolCall;

  #[test]
  fn truncate_keeps_system_and_drops_oldest_turns() {
    let mut msgs = vec![ChatMessage::system("sys prompt here")];
    for i in 1..=8 {
      msgs.push(ChatMessage::user(format!("user turn number {i} with some body text")));
      msgs.push(ChatMessage::assistant(format!("assistant answer number {i} with some body text")));
    }
    let counts: Vec<usize> = msgs.iter().map(|m| (msg_chars(m) / 4) + 1).collect();
    let (small, small_sum) = truncate_messages(msgs.clone(), &counts, 30);
    assert_eq!(small[0].role, Role::System);
    assert!(small.len() < msgs.len(), "old turns dropped: {} -> {}",
      msgs.len(), small.len());
    assert!(small_sum <= 30, "kept sum must respect the budget: {small_sum}");
    let last = small.last().unwrap();
    assert_eq!(last.content.as_deref(), Some("assistant answer number 8 with some body text"));
    assert!(matches!(small[1].role, Role::User | Role::Assistant));

    let (big, big_sum) = truncate_messages(msgs.clone(), &counts, 1_000_000);
    assert_eq!(big.len(), msgs.len());
    assert_eq!(big_sum, counts.iter().sum::<usize>());
  }

  #[test]
  fn truncate_cuts_tool_rounds_as_a_pair() {
    let mut msgs = vec![ChatMessage::system("sys")];
    for i in 1..=4 {
      msgs.push(ChatMessage::user(format!("u{i}")));
      let call = ToolCall::new(format!("id{i}"), "bash".to_string(), "{}".to_string());
      let mut asst = ChatMessage::assistant(format!("thinking then call {i}"));
      asst.tool_calls = Some(vec![call.clone()]);
      msgs.push(asst);
      msgs.push(ChatMessage::tool(call.id.clone(), "bash".to_string(), "out".to_string()));
    }
    let counts: Vec<usize> = msgs.iter().map(|m| (msg_chars(m) / 4) + 1).collect();
    let (small, _) = truncate_messages(msgs.clone(), &counts, 6);
    assert!(matches!(small[1].role, Role::User | Role::Assistant));
    let has_call = small.iter().any(|m| m.tool_calls.is_some());
    let has_result = small.iter().any(|m| m.role == Role::Tool);
    assert_eq!(has_call, has_result, "tool round is atomic");
  }

  #[test]
  fn oversize_minimal_window_is_returned_whole_with_full_sum() {
    let msgs = vec![ChatMessage::system("s"), ChatMessage::user("giant turn")];
    let counts = vec![1, 1000];
    let (out, sum) = truncate_messages(msgs, &counts, 500);
    assert_eq!(out.len(), 2, "nothing left to drop");
    assert_eq!(sum, 1001, "caller must see the budget violation");
  }
}
