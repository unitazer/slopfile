use crate::api::LlamaClient;
use crate::session::Session;
use crate::types::{AppConfig, ChatMessage, Endpoint, PromptSample, Role};

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
    // Same request builder as every other route on this endpoint, so the provider's headers go
    // out with it. A llama-server behind an authenticating proxy would otherwise 401 here four
    // times a turn, and the only symptom is a warning -- which silently disables budget
    // enforcement for the whole session.
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
  let mut chunks = todo.chunks(FAN_OUT);
  while let Some(batch) = chunks.next() {
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
  pub messages: Vec<ChatMessage>,
  pub dropped: usize,
  /// `None` when nothing has been measured and nothing is being guessed -- the first turn
  /// against an endpoint with no tokenizer. Reporting a number there would be inventing one, and
  /// every downstream use (the pressure line, the log) has to be able to say so.
  pub token_count: Option<usize>,
  /// Bytes of the payload that will actually be sent, to be paired with `usage.prompt_tokens`
  /// once the response comes back. That pair is what the next turn measures against.
  pub payload_chars: u64,
}

/// The room left for the prompt: the model has to fit the prompt *and* the answer in the window,
/// so the prompt may only use the budget minus the output allowance.
pub fn prompt_budget(budget: u32) -> u32 {
  budget.saturating_sub(output_allowance(budget))
}

pub fn output_allowance(budget: u32) -> u32 {
  // The 40k floor is what a real reply costs, but it must never eat the entire window: a budget
  // at or below it would leave nothing for the prompt, and every turn would fail with "request
  // exceeds the context budget" no matter how short the transcript. Capped at three quarters so
  // any budget above the floor behaves exactly as before and only the broken range changes.
  (budget / 4).max(24_000).min(budget - budget / 4)
}

/// How to turn a message list into a size, and what we last learned from the provider.
#[derive(Debug, Clone, Default)]
pub struct Counting {
  /// The previous request as the provider counted it. Its ratio is the only density figure we
  /// have, and it is a measured one.
  pub anchor: Option<PromptSample>,
  /// Serialized size of the tool definitions. They are billed as prompt on every request and
  /// belong to no message, so they have to be counted separately -- historically they were not,
  /// which is part of why the old character estimate ran systematically low.
  pub tools_chars: u64,
}

/// Bytes of serialized JSON for one message *as this endpoint will receive it*. This is the unit
/// the anchor is denominated in, and the reason the ratio means something: the provider tokenizes
/// this payload, framing, tool schemas and all, rather than the prose on its own.
///
/// It has to be the wire form and not the stored form. A hosted endpoint has `reasoning_content`
/// stripped before the request goes out, and reasoning is most of the payload on a reasoning
/// model -- measuring the stored form overstates `chars` while `tokens` stays correct, which
/// inflates the ratio and under-reports every prompt from then on. Reasoning is persisted in the
/// transcript and replayed into every request, so the bias is refreshed on every turn rather
/// than washing out.
fn message_chars(m: &ChatMessage, llama: bool) -> u64 {
  // Sanitized one message at a time so the result stays index-aligned with the list: the
  // whole-list form drops degenerate entries, which would shift every later count.
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

/// Scale the payload by the measured ratio. `None` means there is nothing to scale by, and the
/// honest response to that is to enforce nothing -- not to substitute a constant.
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
  // The tool definitions ride on the system message, which `truncate_messages` never drops, so
  // putting them here keeps them counted in every branch that keeps anything.
  if let Some(first) = counts.first_mut() {
    *first += (counting.tools_chars as f64 / ratio).round() as usize;
  }
  Some(counts)
}

pub async fn build_request_messages(
  session: &Session,
  messages: Vec<ChatMessage>,
  cfg: &AppConfig,
  endpoint: &Endpoint,
  http: &LlamaClient,
  counting: &Counting,
) -> Result<RequestPlan, String> {
  if messages.is_empty() {
    return Ok(RequestPlan {
      messages,
      dropped: 0,
      token_count: None,
      payload_chars: 0,
    });
  }
  // llama-server can count for real. Everything else has to be measured from the provider's own
  // usage, which means the very first turn against a new endpoint has nothing to go on.
  let model = session.model().unwrap_or(&cfg.default_session_model);
  let counts = if endpoint.llama {
    match request_token_counts(endpoint, http, model, &messages).await {
      Ok(counts) => Some(counts),
      Err(e) => {
        tracing::warn!("{e}; no measured sample either, so this turn enforces no budget");
        estimate_counts(&messages, counting, endpoint.llama, &endpoint.base, model)
      }
    }
  } else {
    estimate_counts(&messages, counting, endpoint.llama, &endpoint.base, model)
  };

  let Some(counts) = counts else {
    let payload = payload_chars(&messages, counting.tools_chars, endpoint.llama);
    return Ok(RequestPlan {
      messages,
      dropped: 0,
      token_count: None,
      payload_chars: payload,
    });
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
    token_count: Some(kept_sum),
    payload_chars: payload_chars(&truncated, counting.tools_chars, endpoint.llama),
    messages: truncated,
  })
}

/// The two sizes the model is told to respect, derived from what it will actually get.
///
/// `usable` rather than the configured window, and that is the whole point: `output_allowance`
/// reserves up to 40k of the window for the reply, so at a 65k window the prompt only ever gets
/// 25536. Thresholds computed from the configured number would then sit *above* the ceiling the
/// model is truncated at -- advice it can act on and still be cut off, which is worse than no
/// advice at all. `lo` is where to start pruning; `hi` is the hard ceiling to stay under no
/// matter how much it wants to keep.
pub fn context_bands(budget: u32) -> (u32, u32) {
  let usable = prompt_budget(budget);
  // `max(20_000)` would exceed a genuinely small window, so the ceiling always wins: a number the
  // model cannot actually reach is advice it can act on and still be truncated. The floors below
  // only apply where there is room for them.
  let hi = (usable / 2).max(20_000).min(usable);
  let lo = (usable / 4).clamp(8_000, 80_000).min(hi / 2);
  (lo, hi)
}

pub fn context_budget(session: &Session, cfg: &AppConfig) -> u32 {
  session
    .config
    .max_context_tokens
    .unwrap_or(cfg.max_context_tokens)
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

  /// `/tokenize` is llama.cpp's own route. A hosted provider has no such thing, so probing it
  /// costs four doomed requests per turn to learn what we already know from the config.
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

    // Port 1 is the tcpmux port: nothing listens, so this fails fast and proves the probe ran.
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
      msgs.to_vec(),
      cfg,
      endpoint,
      &crate::api::test_client_at(endpoint.base.clone()),
      &counting,
    )
    .await
    .expect("a plan")
  }

  /// Five turns of prose, comfortably over a 200k budget once tokenized at 4 bytes/token.
  fn fat_transcript() -> Vec<ChatMessage> {
    let mut msgs = vec![ChatMessage::system("you are an assistant")];
    for _ in 1..=5 {
      msgs.push(ChatMessage::user("q".repeat(100_000)));
      msgs.push(ChatMessage::assistant("a".repeat(100_000)));
    }
    msgs
  }

  /// The first turn against a provider with no tokenizer. There is nothing to enforce against, and
  /// substituting a constant is exactly what this replaced.
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
    assert_eq!(plan.dropped, 0, "nothing was truncated");
    assert_eq!(
      plan.messages.len(),
      msgs.len(),
      "a transcript over budget still goes out whole when we cannot measure it"
    );
    assert!(
      plan.payload_chars > 900_000,
      "but its size is known exactly"
    );
  }

  /// Turn two against the same endpoint: the sample from turn one supplies the density, and the
  /// budget is enforced again.
  #[tokio::test]
  async fn a_sample_turns_the_same_transcript_into_a_count_and_enforces_it() {
    let msgs = fat_transcript();
    let chars = payload_chars(&msgs, 0, false);
    let plan = plan_for(
      &msgs,
      &hosted(200_000),
      &hosted_endpoint(),
      &Counting {
        // Turn one went out at 4 bytes per token.
        anchor: Some(PromptSample {
          chars,
          tokens: (chars / 4) as u32,
          endpoint: hosted_endpoint().base,
          model: hosted(200_000).default_session_model,
        }),
        tools_chars: 0,
      },
    )
    .await;
    let count = plan.token_count.expect("measured, so counted");
    assert!(
      plan.dropped > 0,
      "250k tokens will not fit a 200k budget, so the measured count has to bite"
    );
    assert!(
      count <= prompt_budget(200_000) as usize,
      "and what survives has to fit: {count}"
    );
    // The reported number still comes from the measured ratio, not from a constant.
    let kept_chars = payload_chars(&plan.messages, 0, false);
    assert!(
      (count as u64).abs_diff(kept_chars / 4) < kept_chars / 40,
      "kept sum {count} should track kept payload {kept_chars}/4"
    );
  }

  /// The same transcript measured two ways must come out two different sizes. There is no constant
  /// anywhere in here for a denser model to be wrongly held to.
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

  /// The anchor is only meaningful if `chars` counts what the provider actually received. On a
  /// hosted endpoint `reasoning_content` is stripped before the request goes out, and on a
  /// reasoning model that is most of the payload: measuring the stored form inflated a single
  /// 4k-reasoning message 90x, which would have made every later count wildly low and stopped
  /// truncation from ever firing.
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
    // A hosted endpoint drops the reasoning, so it must not be in the denominator.
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

  /// The bias the above prevents, stated as an end-to-end number: a sample taken against the
  /// stored form must not be able to under-report a prompt.
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

  /// The tool definitions are billed as prompt on every request and belong to no message. The old
  /// character estimate never saw them, which is part of why it ran systematically low.
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

  /// llama-server counts for real over `/tokenize`, and that count is content-only by design --
  /// `max_context_tokens` has been hand-tuned against it against `ctx-size`. A sample must not
  /// quietly start feeding that path.
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
      // A wildly wrong sample: if it were consulted, the counts would be absurd.
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

  /// A budget at or below the 40k output floor used to leave nothing for the prompt, so every turn
  /// failed with "exceeds the context budget" no matter how short the transcript was.
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
    // Above the floor nothing changes.
    assert_eq!(output_allowance(65_536), 40_000);
    assert_eq!(prompt_budget(65_536), 25_536);
  }

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
    let counts: Vec<usize> = msgs
      .iter()
      .map(|m| message_chars(m, false) as usize)
      .collect();
    let total: usize = counts.iter().sum();
    // A budget around a third of the transcript, so plenty has to go.
    let cap = total / 3;
    let (small, small_sum) = truncate_messages(msgs.clone(), &counts, cap as u32);
    assert_eq!(small[0].role, Role::System);
    assert!(
      small.len() < msgs.len(),
      "old turns dropped: {} -> {}",
      msgs.len(),
      small.len()
    );
    assert!(
      small_sum <= cap,
      "kept sum must respect the budget: {small_sum} <= {cap}"
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
    let counts: Vec<usize> = msgs
      .iter()
      .map(|m| message_chars(m, false) as usize)
      .collect();
    // Enough for the system prompt and roughly one round, but not all four.
    let (small, _) = truncate_messages(msgs.clone(), &counts, (counts[0] + counts[1] * 4) as u32);
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
