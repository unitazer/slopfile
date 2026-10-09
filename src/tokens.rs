use crate::session::Session;
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
    (
      format!("{api_base}/tokenize"),
      serde_json::json!({"content": content.clone(), "model": model}),
    ),
    (
      format!("{api_base}/tokenize"),
      serde_json::json!({"content": content.clone()}),
    ),
    (
      format!("{api_base}/v1/tokenize"),
      serde_json::json!({"content": content.clone(), "model": model}),
    ),
    (
      format!("{api_base}/v1/tokenize"),
      serde_json::json!({"content": content.clone()}),
    ),
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

/// Token counts are cached by content: the system prompt and unchanged tool results dominate a
/// long session, and re-tokenizing them on every turn is pure overhead.
#[derive(Default)]
struct CountCache {
  counts: std::collections::HashMap<u64, usize>,
}

fn content_key(m: &ChatMessage) -> u64 {
  let mut hash: u64 = 0xcbf29ce484222325;
  let mut eat = |s: &str| {
    for b in s.bytes() {
      hash ^= b as u64;
      hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= 0xff;
    hash = hash.wrapping_mul(0x100000001b3);
  };
  if let Some(c) = &m.content {
    eat(c);
  }
  if let Some(r) = &m.reasoning_content {
    eat(r);
  }
  if let Some(calls) = &m.tool_calls {
    for c in calls {
      eat(&c.id);
      eat(&c.name);
      eat(&c.arguments);
    }
  }
  if let Some(id) = &m.tool_call_id {
    eat(id);
  }
  hash
}

async fn request_token_counts(
  api_base: &str,
  client: &reqwest::Client,
  model: &str,
  messages: &[ChatMessage],
) -> Result<Vec<usize>, String> {
  let mut cache = CountCache::default();
  let mut todo: Vec<(usize, u64)> = messages
    .iter()
    .enumerate()
    .map(|(ix, m)| (ix, content_key(m)))
    .filter(|(_, key)| !cache.counts.contains_key(key))
    .collect();
  todo.dedup_by(|a, b| a.1 == b.1);
  let mut error = None;
  const FAN_OUT: usize = 8;
  let mut chunks = todo.chunks(FAN_OUT);
  while let Some(batch) = chunks.next() {
    let futs = batch.iter().map(|(ix, key)| async move {
      let counted = request_token_count(
        api_base,
        client,
        model,
        std::slice::from_ref(&messages[*ix]),
      )
      .await;
      (*key, counted)
    });
    for (key, counted) in futures_util::future::join_all(futs).await {
      match counted {
        Ok(n) => {
          cache.counts.insert(key, n);
        }
        Err(e) => error = Some(e),
      }
    }
  }
  messages
    .iter()
    .map(|m| {
      cache.counts.get(&content_key(m)).copied().ok_or_else(|| {
        error
          .clone()
          .unwrap_or_else(|| "no token count".to_string())
      })
    })
    .collect()
}

#[derive(Debug)]
pub struct RequestPlan {
  pub messages: Vec<ChatMessage>,
  pub dropped: usize,
  pub token_count: usize,
}

/// The room left for the prompt: the model has to fit the prompt *and* the answer in the window,
/// so the prompt may only use the budget minus the output allowance.
pub fn prompt_budget(budget: u32) -> u32 {
  budget.saturating_sub(output_allowance(budget))
}

pub fn output_allowance(budget: u32) -> u32 {
  budget / 4
}

pub async fn build_request_messages(
  session: &Session,
  messages: Vec<ChatMessage>,
  cfg: &AppConfig,
  http: &reqwest::Client,
) -> Result<RequestPlan, String> {
  if messages.is_empty() {
    return Ok(RequestPlan {
      messages,
      dropped: 0,
      token_count: 0,
    });
  }
  // A tokenizer that is down must not take the session down with it: fall back to a rough
  // estimate and let the model keep working.
  let counts = match request_token_counts(
    &cfg.api_base,
    http,
    session.model().unwrap_or(&cfg.model),
    &messages,
  )
  .await
  {
    Ok(counts) => counts,
    Err(e) => {
      tracing::warn!("{e}; falling back to a 4-chars-per-token estimate");
      messages
        .iter()
        .map(|m| (estimate_chars(m) / 4) + 1)
        .collect()
    }
  };
  let cap = prompt_budget(context_budget(session, cfg));
  let (truncated, kept_sum) = truncate_messages(messages, &counts, cap);
  if kept_sum > cap as usize {
    return Err(format!(
      "request exceeds the context budget even after dropping old turns: {kept_sum} > {cap} tokens; trim the transcript"
    ));
  }
  Ok(RequestPlan {
    dropped: counts.len() - truncated.len(),
    token_count: kept_sum,
    messages: truncated,
  })
}

pub fn context_budget(session: &Session, cfg: &AppConfig) -> u32 {
  session
    .config
    .max_context_tokens
    .unwrap_or(cfg.max_context_tokens)
}

pub(crate) fn estimate_chars(m: &ChatMessage) -> usize {
  m.content.as_ref().map_or(0, |c| c.len())
    + m.reasoning_content.as_ref().map_or(0, |c| c.len())
    + m
      .tool_calls
      .as_ref()
      .map_or(0, |t| t.iter().map(|c| c.arguments.len() + 16).sum())
    + m.name.as_ref().map_or(0, |n| n.len())
    + m.tool_call_id.as_ref().map_or(0, |n| n.len())
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
  // Every adjustment below may only move keep_from *forward*. Moving it backward keeps more
  // messages and can push the window back over budget, which the caller reports as a hard error
  // and then parks the session. Forward-only moves can only free budget, so the window always fits.
  // A valid window must open on a `user` message. Opening on an assistant or on a bare tool
  // result is rejected by chat templates that expect strict alternation.
  let budget_keep_from = keep_from;
  if !matches!(messages.get(keep_from).map(|m| m.role), Some(Role::User)) {
    if let Some(next) = (keep_from..messages.len()).find(|ix| messages[*ix].role == Role::User) {
      keep_from = next;
    } else if let Some(prev) = (1..keep_from)
      .rev()
      .find(|ix| messages[*ix].role == Role::User)
    {
      // May exceed the budget; the caller then refuses the request, which is the honest outcome.
      keep_from = prev;
    } else {
      // No valid window fits. Report the over-budget one honestly instead of silently degrading
      // to a bare system prompt that asks the model to continue with no conversation at all.
      keep_from = budget_keep_from;
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::types::ToolCall;

  #[test]
  fn truncate_keeps_system_and_drops_oldest_turns() {
    let mut msgs = vec![ChatMessage::system("sys prompt here")];
    for i in 1..=8 {
      msgs.push(ChatMessage::user(format!(
        "user turn number {i} with some body text"
      )));
      msgs.push(ChatMessage::assistant(format!(
        "assistant answer number {i} with some body text"
      )));
    }
    let counts: Vec<usize> = msgs.iter().map(|m| (estimate_chars(m) / 4) + 1).collect();
    let (small, small_sum) = truncate_messages(msgs.clone(), &counts, 30);
    assert_eq!(small[0].role, Role::System);
    assert!(
      small.len() < msgs.len(),
      "old turns dropped: {} -> {}",
      msgs.len(),
      small.len()
    );
    assert!(
      small_sum <= 30,
      "kept sum must respect the budget: {small_sum}"
    );
    let last = small.last().unwrap();
    assert_eq!(
      last.content.as_deref(),
      Some("assistant answer number 8 with some body text")
    );
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
      msgs.push(ChatMessage::tool(
        call.id.clone(),
        "bash".to_string(),
        "out".to_string(),
      ));
    }
    let counts: Vec<usize> = msgs.iter().map(|m| (estimate_chars(m) / 4) + 1).collect();
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
