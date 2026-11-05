use crate::api::LlamaClient;
use crate::session::Session;
use crate::types::{AppConfig, ChatMessage, Endpoint, PromptSample, SessionConfig};
pub(crate) async fn request_token_count(
  endpoint: &Endpoint,
  client: &LlamaClient,
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
  if !endpoint.llama {
    return Err(format!(
      "{} is not a llama-server, so it has no /tokenize",
      endpoint.base
    ));
  }
  let mut last_err = String::new();
  for (url, body) in [
    (
      endpoint.raw_url("tokenize"),
      serde_json::json!({"content": content.clone(), "model": model}),
    ),
    (
      endpoint.raw_url("tokenize"),
      serde_json::json!({"content": content.clone()}),
    ),
    (
      endpoint.url("tokenize"),
      serde_json::json!({"content": content.clone(), "model": model}),
    ),
    (
      endpoint.url("tokenize"),
      serde_json::json!({"content": content.clone()}),
    ),
  ] {




    let resp = match client.request(&url).json(&body).send().await {
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
  endpoint: &Endpoint,
  client: &LlamaClient,
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
  let chunks = todo.chunks(FAN_OUT);
  for batch in chunks {
    let futs = batch.iter().map(|(ix, key)| async move {
      let counted = request_token_count(
        endpoint,
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
  pub token_count: Option<usize>,
  pub payload_chars: u64,
}
pub fn prompt_budget(budget: u32) -> u32 {
  budget.saturating_sub(output_allowance(budget))
}
pub fn output_allowance(budget: u32) -> u32 {
  (budget / 4).max(32_000).min(budget - budget / 4)
}
#[derive(Debug, Clone, Default)]
pub struct Counting {
  pub anchor: Option<PromptSample>,
  pub tools_chars: u64,
}
///
fn message_chars(m: &ChatMessage, llama: bool) -> u64 {


  crate::api::sanitize_messages(std::slice::from_ref(m), llama)
    .first()
    .map_or(0, |m| {
      serde_json::to_string(m).map_or(0, |s| s.len() as u64)
    })
}
fn payload_chars(messages: &[ChatMessage], tools_chars: u64, llama: bool) -> u64 {
  messages
    .iter()
    .map(|m| message_chars(m, llama))
    .sum::<u64>()
    + tools_chars
}
fn estimate_counts(
  messages: &[ChatMessage],
  counting: &Counting,
  llama: bool,
  endpoint: &str,
  model: &str,
) -> Option<Vec<usize>> {
  let ratio = counting
    .anchor
    .as_ref()
    .and_then(|a| a.ratio_for(endpoint, model))?;
  let mut counts: Vec<usize> = messages
    .iter()
    .map(|m| (message_chars(m, llama) as f64 / ratio).round() as usize)
    .collect();


  if let Some(first) = counts.first_mut() {
    *first += (counting.tools_chars as f64 / ratio).round() as usize;
  }
  Some(counts)
}
pub async fn build_request_messages(
  session: &Session,
  messages: &[ChatMessage],
  cfg: &AppConfig,
  endpoint: &Endpoint,
  http: &LlamaClient,
  counting: &Counting,
) -> Result<RequestPlan, String> {
  if messages.is_empty() {
    return Ok(RequestPlan {
      token_count: None,
      payload_chars: 0,
    });
  }


  let model = session.model().unwrap_or(&cfg.default_session_model);
  let counts = if endpoint.llama {
    match request_token_counts(endpoint, http, model, messages).await {
      Ok(counts) => Some(counts),
      Err(e) => {
        tracing::warn!("{e}; no measured sample either, so this turn enforces no budget");
        estimate_counts(messages, counting, endpoint.llama, &endpoint.base, model)
      }
    }
  } else {
    estimate_counts(messages, counting, endpoint.llama, &endpoint.base, model)
  };
  let Some(counts) = counts else {
    let payload = payload_chars(messages, counting.tools_chars, endpoint.llama);
    return Ok(RequestPlan {
      token_count: None,
      payload_chars: payload,
    });
  };
  let cap = prompt_budget(context_budget(&session.config, cfg));
  let total: usize = counts.iter().sum();
  if total > cap as usize {
    return Err(format!(
      "request exceeds the context budget: {total} > {cap} tokens; compact the transcript"
    ));
  }
  Ok(RequestPlan {
    token_count: Some(total),
    payload_chars: payload_chars(messages, counting.tools_chars, endpoint.llama),
  })
}
///
pub fn context_bands(budget: u32) -> (u32, u32) {
  let usable = prompt_budget(budget);



  let hi = (usable / 2).max(20_000).min(usable);
  let lo = (usable / 4).clamp(8_000, 80_000).min(hi / 2);
  (lo, hi)
}
pub fn context_budget(session_config: &SessionConfig, cfg: &AppConfig) -> u32 {
  session_config
    .max_context_tokens
    .unwrap_or(cfg.max_context_tokens)
}
#[cfg(test)]
mod tests {
  use super::*;
  #[tokio::test]
  async fn only_a_llama_server_is_asked_to_tokenize() {
    let msgs = [ChatMessage::user("hello")];
    let hosted = Endpoint {
      base: "http://127.0.0.1:1".to_string(),
      headers: Default::default(),
      llama: false,
      reasoning_style: crate::types::ReasoningStyle::Effort,
    };
    let err = request_token_count(
      &hosted,
      &crate::api::test_client_at("http://127.0.0.1:1".to_string()),
      "m",
      &msgs,
    )
    .await
    .unwrap_err();
    assert!(err.contains("not a llama-server"), "{err}");

    let llama = Endpoint::local("http://127.0.0.1:1");
    let err = request_token_count(
      &llama,
      &crate::api::test_client_at(llama.base.clone()),
      "m",
      &msgs,
    )
    .await
    .unwrap_err();
    assert!(err.contains("/tokenize"), "{err}");
  }
  fn hosted(budget: u32) -> AppConfig {
    AppConfig {
      max_context_tokens: budget,
      ..Default::default()
    }
  }
  fn hosted_endpoint() -> Endpoint {
    Endpoint {
      base: "http://127.0.0.1:1".to_string(),
      headers: Default::default(),
      llama: false,
      reasoning_style: crate::types::ReasoningStyle::Effort,
    }
  }
  async fn plan_for(
    msgs: &[ChatMessage],
    cfg: &AppConfig,
    endpoint: &Endpoint,
    counting: &Counting,
  ) -> RequestPlan {
    build_request_messages(
      &Session::default(),
      msgs,
      cfg,
      endpoint,
      &crate::api::test_client_at(endpoint.base.clone()),
      counting,
    )
    .await
    .expect("a plan")
  }
  fn fat_transcript() -> Vec<ChatMessage> {
    let mut msgs = vec![ChatMessage::system("you are an assistant")];
    for _ in 1..=5 {
      msgs.push(ChatMessage::user("q".repeat(100_000)));
      msgs.push(ChatMessage::assistant("a".repeat(100_000)));
    }
    msgs
  }
  #[tokio::test]
  async fn with_no_sample_the_first_turn_enforces_nothing_and_reports_nothing() {
    let msgs = fat_transcript();
    let plan = plan_for(
      &msgs,
      &hosted(200_000),
      &hosted_endpoint(),
      &Counting {
        anchor: None,
        tools_chars: 0,
      },
    )
    .await;
    assert_eq!(plan.token_count, None, "no basis, no number");
    assert!(
      plan.payload_chars > 900_000,
      "but its size is known exactly"
    );
  }
  #[tokio::test]
  async fn a_sample_turns_the_same_transcript_into_an_over_budget_refusal() {
    let msgs = fat_transcript();
    let chars = payload_chars(&msgs, 0, false);
    let cfg = hosted(200_000);
    let endpoint = hosted_endpoint();
    let err = build_request_messages(
      &Session::default(),
      &msgs,
      &cfg,
      &endpoint,
      &crate::api::test_client_at(endpoint.base.clone()),
      &Counting {

        anchor: Some(PromptSample {
          chars,
          tokens: (chars / 4) as u32,
          endpoint: endpoint.base.clone(),
          model: cfg.default_session_model.clone(),
        }),
        tools_chars: 0,
      },
    )
    .await
    .unwrap_err();
    assert!(err.contains("exceeds the context budget"), "{err}");
    assert!(
      err.contains("compact the transcript"),
      "the model is told what to do instead: {err}"
    );
  }
  #[test]
  fn the_estimate_is_exactly_the_payload_over_the_measured_ratio() {
    let msgs = fat_transcript();
    let chars = payload_chars(&msgs, 0, false);
    let at = |tokens: u32| {
      estimate_counts(
        &msgs,
        &Counting {
          anchor: Some(PromptSample {
            chars,
            tokens,
            endpoint: "http://x".into(),
            model: "m".into(),
          }),
          tools_chars: 0,
        },
        false,
        "http://x",
        "m",
      )
      .unwrap()
      .iter()
      .sum::<usize>()
    };
    let at_four = at((chars / 4) as u32);
    let at_two = at((chars / 2) as u32);
    assert!(at_four.abs_diff(chars as usize / 4) < 10, "{at_four}");
    assert!(at_two.abs_diff(chars as usize / 2) < 10, "{at_two}");
    assert!(
      at_two > at_four * 3 / 2,
      "half the bytes per token must mean far more tokens: {at_four} -> {at_two}"
    );
  }
  #[test]
  fn the_payload_measured_is_the_payload_sent() {
    let mut m = ChatMessage::user("what is the answer?");
    m.reasoning_content = Some("x".repeat(4000));
    let msgs = vec![ChatMessage::system("sys"), m.clone()];
    for llama in [true, false] {
      let measured = payload_chars(&msgs, 0, llama);
      let sent = payload_chars(&crate::api::sanitize_messages(&msgs, llama), 0, llama);
      assert_eq!(
        measured, sent,
        "llama={llama}: measuring {measured} against {sent} sent"
      );
    }

    assert!(
      payload_chars(&msgs, 0, false) < 500,
      "hosted: {} bytes must exclude 4000 of reasoning",
      payload_chars(&msgs, 0, false)
    );
    assert!(
      payload_chars(&msgs, 0, true) > 4000,
      "llama-server keeps reasoning and counts it, as it always has"
    );
  }
  #[test]
  fn a_reasoning_heavy_sample_cannot_under_report() {
    let mut m = ChatMessage::assistant("the answer is 42");
    m.reasoning_content = Some("thinking".repeat(1000));
    let msgs = vec![ChatMessage::system("sys"), m];
    let chars = payload_chars(&msgs, 0, false);
    let counts = estimate_counts(
      &msgs,
      &Counting {
        anchor: Some(PromptSample {
          chars,
          tokens: (chars / 4) as u32,
          endpoint: "http://x".into(),
          model: "m".into(),
        }),
        tools_chars: 0,
      },
      false,
      "http://x",
      "m",
    )
    .unwrap();
    let total: usize = counts.iter().sum();
    assert!(
      (total as u64).abs_diff(chars / 4) < 10,
      "measured {chars} bytes, counted {total} tokens, expected ~{}",
      chars / 4
    );
  }
  #[tokio::test]
  async fn tool_definitions_are_counted_as_prompt() {
    let msgs = vec![ChatMessage::system("sys"), ChatMessage::user("hello")];
    let anchor = || {
      Some(PromptSample {
        chars: 4000,
        tokens: 1000,
        endpoint: hosted_endpoint().base,
        model: hosted(200_000).default_session_model,
      })
    };
    let without = plan_for(
      &msgs,
      &hosted(200_000),
      &hosted_endpoint(),
      &Counting {
        anchor: anchor(),
        tools_chars: 0,
      },
    )
    .await;
    let with = plan_for(
      &msgs,
      &hosted(200_000),
      &hosted_endpoint(),
      &Counting {
        anchor: anchor(),
        tools_chars: 4000,
      },
    )
    .await;
    assert_eq!(
      with.token_count.unwrap() - without.token_count.unwrap(),
      1000,
      "4000 bytes of tool schemas at 4 bytes/token is 1000 tokens of prompt"
    );
  }
  #[tokio::test]
  async fn a_llama_server_is_counted_by_its_own_tokenizer_not_the_sample() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const REPORTED: usize = 777;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
      for _ in 0..4 {
        let Ok((mut sock, _)) = listener.accept().await else {
          return;
        };
        let mut buf = vec![0u8; 32 * 1024];
        let _ = sock.read(&mut buf).await;
        let body = format!("{{\"tokens\":[{}]}}", vec!["0"; REPORTED].join(","));
        let resp = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
          body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
      }
    });
    let msgs = fat_transcript();
    let plan = plan_for(
      &msgs,
      &hosted(200_000),
      &Endpoint::local(&format!("http://{addr}")),

      &Counting {
        anchor: Some(PromptSample {
          chars: 999_999,
          tokens: 1,
          endpoint: "http://x".into(),
          model: "m".into(),
        }),
        tools_chars: 0,
      },
    )
    .await;
    assert_eq!(
      plan.token_count,
      Some(REPORTED * msgs.len()),
      "the tokenizer's own answer, one per message"
    );
  }
  #[test]
  fn a_small_budget_still_leaves_room_for_a_prompt() {
    for budget in [8_192u32, 40_000, 65_536, 200_000] {
      let allowance = output_allowance(budget);
      assert!(
        allowance < budget,
        "a {budget}-token window cannot spend {allowance} on the reply alone"
      );
      assert!(
        prompt_budget(budget) > 0,
        "and must leave something to send at {budget}"
      );
    }

    assert_eq!(output_allowance(65_536), 32_000);
    assert_eq!(prompt_budget(65_536), 33_536);
  }
}
