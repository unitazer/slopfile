use crate::sse::{SseError, stream_chunks};
use crate::types::{
  ChatMessage, Role, SseToolCallDelta, StreamEvent, StreamResult, Thinking, ToolCall,
  ToolDefinition,
};
use futures_util::StreamExt;
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

const KEEP_GOING_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum ApiError {
  Reqwest(reqwest::Error),
  NonOkStatus(u16, String),
  Sse(SseError),
  Timeout(std::time::Duration),
}

impl fmt::Display for ApiError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ApiError::Reqwest(e) => write!(f, "http error: {e}"),
      ApiError::NonOkStatus(code, body) => write!(f, "server returned {code}: {body}"),
      ApiError::Sse(e) => write!(f, "sse error: {e}"),
      ApiError::Timeout(d) => write!(f, "request timed out after {d:?}"),
    }
  }
}

impl Error for ApiError {}

impl From<reqwest::Error> for ApiError {
  fn from(e: reqwest::Error) -> Self {
    ApiError::Reqwest(e)
  }
}

impl From<SseError> for ApiError {
  fn from(e: SseError) -> Self {
    ApiError::Sse(e)
  }
}

pub const MAX_TOOL_CALLS: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct ToolCallAccumulator {
  calls: Vec<SseToolCallDelta>,
}

impl ToolCallAccumulator {
  pub fn accumulate(&mut self, delta: &SseToolCallDelta) {
    // A single frame decides how big this Vec gets, and the endpoint is not trusted to be
    // sane: `index: 10000000` would otherwise allocate gigabytes.
    let idx = delta.index.unwrap_or(0) as usize;
    let idx = idx.min(MAX_TOOL_CALLS - 1);
    while self.calls.len() <= idx {
      self.calls.push(SseToolCallDelta {
        index: Some(self.calls.len()),
        id: None,
        r#type: None,
        function: None,
      });
    }
    let slot = &mut self.calls[idx];
    if let Some(id) = &delta.id {
      slot.id = Some(id.clone());
    }
    if let Some(t) = &delta.r#type {
      slot.r#type = Some(t.clone());
    }
    match (&delta.function, &mut slot.function) {
      (Some(f), Some(sf)) => {
        if let Some(name) = &f.name {
          sf.name = Some(name.clone());
        }
        if let Some(args) = &f.arguments {
          sf.arguments = Some(format!("{}{}", sf.arguments.as_deref().unwrap_or(""), args));
        }
      }
      (Some(f), None) => slot.function = Some(f.clone()),
      _ => {}
    }
  }

  pub fn assemble(&self) -> Vec<ToolCall> {
    let mut taken: Vec<String> = Vec::new();
    self
      .calls
      .iter()
      .enumerate()
      .filter_map(|(ix, c)| {
        let name = c.function.as_ref()?.name.clone()?;
        // An endpoint that omits ids would otherwise pair two results with the same empty
        // `tool_call_id`, and the model cannot tell which output belongs to which call.
        let mut id = c.id.clone().unwrap_or_default();
        if id.trim().is_empty() || taken.contains(&id) {
          id = format!("{name}-{ix}");
        }
        taken.push(id.clone());
        Some(ToolCall::new(
          id,
          name,
          c.function
            .as_ref()
            .and_then(|f| f.arguments.clone())
            .unwrap_or_default(),
        ))
      })
      .collect()
  }
}

pub struct LlamaClient {
  base: String,
  client: reqwest::Client,
  idle_timeout: Duration,
}

impl LlamaClient {
  pub fn new(api_base: &str) -> Self {
    Self {
      base: api_base.trim_end_matches('/').to_string(),
      client: reqwest::Client::new(),
      idle_timeout: Duration::from_secs(300),
    }
  }

  pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
    self.idle_timeout = idle_timeout;
    self
  }

  pub fn rest(&self) -> &reqwest::Client {
    &self.client
  }

  pub async fn ensure_model_loaded(&self, model: &str) {
    let body = serde_json::json!({ "model": model });
    let mut last_err = String::new();
    for path in ["models/load", "v1/models/load"] {
      let url = format!("{}/{path}", self.base);
      match self.client.post(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => return,
        Ok(resp) => last_err = format!("{}: HTTP {}", url, resp.status()),
        Err(e) => last_err = format!("{url}: {e}"),
      }
    }
    tracing::warn!(
      "ensure_model_loaded({model}) failed: {last_err}; continuing with whatever is loaded"
    );
  }

  pub async fn chat_stream(
    &self,
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[ToolDefinition]>,
    max_new_tokens: u32,
    thinking: Thinking,
    on_event: impl FnMut(StreamEvent),
    keep_going: impl FnMut() -> bool,
  ) -> Result<StreamResult, ApiError> {
    let body = serde_json::json!({
        "model": model,
        "messages": sanitize_messages(messages),
        "stream": true,
        "max_tokens": max_new_tokens,
    });
    let mut body = body;
    if let Some(tools) = tools {
      body["tools"] = serde_json::to_value(tools)
        .map_err(|e| ApiError::Sse(SseError::Io(format!("tool serialization: {e}"))))?;
    }
    // Omitted for `Auto` so the server keeps its own default. Sent otherwise: llama-server
    // forwards it to the chat template as `reasoning_effort`, and maps `none` to
    // enable_thinking=false.
    if let Some(effort) = thinking.wire_value() {
      body["reasoning_effort"] = serde_json::Value::from(effort);
    }

    let url = format!("{}/v1/chat/completions", self.base);
    let resp = tokio::time::timeout(
      std::time::Duration::from_secs(60),
      self.client.post(&url).json(&body).send(),
    )
    .await
    .map_err(|_| ApiError::Timeout(std::time::Duration::from_secs(60)))?
    .map_err(ApiError::Reqwest)?;

    let status = resp.status();
    if !status.is_success() {
      let snippet = resp.text().await.unwrap_or_default();
      return Err(ApiError::NonOkStatus(status.as_u16(), snippet));
    }

    self
      .consume_stream(resp.bytes_stream(), on_event, keep_going)
      .await
  }

  pub async fn consume_stream<S>(
    &self,
    raw: S,
    mut on_event: impl FnMut(StreamEvent),
    mut keep_going: impl FnMut() -> bool,
  ) -> Result<StreamResult, ApiError>
  where
    S: futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
  {
    let mut result = StreamResult::default();
    let mut acc = ToolCallAccumulator::default();
    let mut chunk_stream = stream_chunks(raw);
    let mut last_data = Instant::now();

    loop {
      tokio::select! {
          chunk = chunk_stream.next() => {
              let Some(chunk) = chunk else { break; };
              // An abort must be honored as soon as the next chunk lands, not only when the
              // idle poll happens to fire.
              if !keep_going() {
                  result.aborted = true;
                  self.cancel_generation().await;
                  break;
              }
              let chunk = chunk.map_err(ApiError::from)?;
              last_data = Instant::now();
              if let Some(usage) = &chunk.usage {
                  result.usage = Some(usage.clone());
              }
              for choice in &chunk.choices {
                  if let Some(fr) = &choice.finish_reason {
                      result.finish_reason = Some(fr.clone());
                  }
                  let d = &choice.delta;
                  if let Some(text) = &d.content {
                      result.content.push_str(text);
                      on_event(StreamEvent::Text(text.clone()));
                  }
                  if let Some(r) = &d.reasoning_content {
                      result.reasoning.push_str(r);
                      on_event(StreamEvent::Reasoning(r.clone()));
                  }
                  if let Some(tools) = &d.tool_calls {
                      for t in tools {
                          acc.accumulate(t);
                          on_event(StreamEvent::ToolCallFragment(t.clone()));
                      }
                  }
              }
          }
          _ = tokio::time::sleep(KEEP_GOING_INTERVAL) => {
              if !keep_going() {
                  result.aborted = true;
                  self.cancel_generation().await;
                  break;
              }
              if last_data.elapsed() >= self.idle_timeout {
                  self.cancel_generation().await;
                  return Err(ApiError::Timeout(last_data.elapsed()));
              }
          }
      }
    }

    result.tool_calls = acc.assemble();
    on_event(StreamEvent::Done(result.clone()));
    Ok(result)
  }

  pub async fn cancel_generation(&self) {
    let url = format!("{}/abort", self.base);
    match self
      .client
      .post(&url)
      .timeout(std::time::Duration::from_millis(2000))
      .send()
      .await
    {
      Ok(_) => tracing::debug!("cancel_generation: {url} → ok"),
      Err(e) => tracing::warn!("cancel_generation: {url}: {e}"),
    }
  }
}

fn sanitize_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
  messages
    .iter()
    .filter(|m| match m.role {
      Role::Assistant => {
        m.content.is_some()
          || m
            .reasoning_content
            .as_deref()
            .is_some_and(|r| !r.is_empty())
          || m.tool_calls.is_some()
      }
      _ => true,
    })
    .map(|m| {
      if m.role == Role::Assistant
        && m.content.is_none()
        && m.tool_calls.is_none()
        && let Some(r) = m.reasoning_content.as_deref().filter(|r| !r.is_empty())
      {
        let mut m2 = m.clone();
        m2.content = Some(r.to_string());
        m2.reasoning_content = None;
        return m2;
      }
      m.clone()
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
  };

  use futures_core::Stream;

  use super::*;
  use crate::types::Usage;

  fn fake_bytes_stream(
    chunks: Vec<&'static str>,
  ) -> impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> {
    let items = chunks
      .into_iter()
      .map(|s| Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(s)))
      .collect::<Vec<_>>();
    futures_util::stream::iter(items)
  }

  struct SlowBytesStream {
    items: std::vec::IntoIter<Result<bytes::Bytes, reqwest::Error>>,
    delay: Option<Pin<Box<tokio::time::Sleep>>>,
  }

  impl SlowBytesStream {
    fn new(chunks: Vec<&'static str>) -> Self {
      let items = chunks
        .into_iter()
        .map(|s| Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(s)))
        .collect::<Vec<_>>()
        .into_iter();
      Self { items, delay: None }
    }
  }

  impl Stream for SlowBytesStream {
    type Item = Result<bytes::Bytes, reqwest::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
      let this = &mut *self;
      if let Some(delay) = this.delay.as_mut() {
        futures_core::ready!(delay.as_mut().poll(cx));
        this.delay = None;
      }
      let Some(item) = this.items.next() else {
        return Poll::Ready(None);
      };
      this.delay = Some(Box::pin(tokio::time::sleep(Duration::from_millis(1000))));
      Poll::Ready(Some(item))
    }
  }

  impl Unpin for SlowBytesStream {}

  #[tokio::test]
  async fn tool_call_fragments_accumulate_end_to_end() {
    let client = LlamaClient::new("http://localhost:9");
    let sse = vec![
      "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"abc123\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"cit\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"y\\\":\\\"Pa\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ris\\\"}\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\n",
      "data: [DONE]\n\n",
    ];
    let mut events = Vec::new();
    let result = client
      .consume_stream(
        fake_bytes_stream(sse),
        |e| {
          events.push(e);
        },
        || true,
      )
      .await
      .expect("stream");

    assert_eq!(result.tool_calls.len(), 1);
    let tc = &result.tool_calls[0];
    assert_eq!(tc.id, "abc123");
    assert_eq!(tc.name, "get_weather");
    assert_eq!(tc.arguments, "{\"city\":\"Paris\"}");
    assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(
      result.usage,
      Some(Usage {
        prompt_tokens: 10,
        completion_tokens: 4
      })
    );
    assert!(
      events
        .iter()
        .any(|e| matches!(e, StreamEvent::ToolCallFragment(_)))
    );
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Done(_))));
  }

  #[tokio::test]
  async fn text_and_reasoning_accumulate() {
    let client = LlamaClient::new("http://localhost:9");
    let sse = vec![
      "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me\"}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" think.\",\"content\":\"Hi\"}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"content\":\" there\"},\"finish_reason\":\"stop\"}]}\n\n",
      "data: [DONE]\n\n",
    ];
    let result = client
      .consume_stream(fake_bytes_stream(sse), |_| {}, || true)
      .await
      .expect("stream");
    assert_eq!(result.content, "Hi there");
    assert_eq!(result.reasoning, "Let me think.");
    assert_eq!(result.finish_reason.as_deref(), Some("stop"));
  }

  #[tokio::test]
  async fn keep_going_returning_false_aborts_the_stream() {
    let client = LlamaClient::new("http://localhost:9");
    let sse = vec![
      "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"content\":\" two\"}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"content\":\" three\"}}]}\n\n",
      "data: [DONE]\n\n",
    ];
    let result = client
      .consume_stream(fake_bytes_stream(sse), |_| {}, || false)
      .await
      .expect("stream");
    assert!(result.aborted, "keep_going is what stops the stream now");
    assert!(
      !result.content.contains(" three"),
      "the stream stops early: {:?}",
      result.content
    );
  }

  #[tokio::test]
  async fn watchdog_aborts_when_keep_going_returns_false() {
    let client = LlamaClient::new("http://localhost:9");
    let sse = vec![
      "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"content\":\" two\"}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"content\":\" three\"}}]}\n\n",
    ];
    let stop = std::cell::Cell::new(false);
    let t0 = std::time::Instant::now();
    let result = client
      .consume_stream(
        SlowBytesStream::new(sse),
        |_| {},
        || {
          if stop.get() {
            return false;
          }
          if t0.elapsed() >= Duration::from_millis(300) {
            stop.set(true);
          }
          !stop.get()
        },
      )
      .await
      .expect("stream");
    assert!(result.aborted, "watchdog should have aborted the stall");
    assert_eq!(result.content, "one");
    assert!(
      t0.elapsed() < std::time::Duration::from_millis(900),
      "abort should fire between 200ms watchdog ticks, not wait for the next chunk"
    );
  }

  #[tokio::test]
  async fn stalled_stream_times_out() {
    let client =
      LlamaClient::new("http://localhost:9").with_idle_timeout(Duration::from_millis(150));
    let sse = vec!["data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n"];
    let result = client
      .consume_stream(SlowBytesStream::new(sse), |_| {}, || true)
      .await;
    assert!(
      matches!(result, Err(ApiError::Timeout(_))),
      "stall must surface as a timeout: {result:?}"
    );
  }

  #[test]
  fn sanitize_keeps_reasoning_only_assistant_messages() {
    let mut reasoning_only = ChatMessage::assistant("");
    reasoning_only.content = None;
    reasoning_only.reasoning_content = Some("Let me think...".to_string());
    let kept = sanitize_messages(&[reasoning_only]);
    assert_eq!(
      kept.len(),
      1,
      "reasoning-only assistant reply must not be dropped"
    );
    let m = &kept[0];
    assert_eq!(m.content.as_deref(), Some("Let me think..."));
    assert!(
      m.reasoning_content.is_none(),
      "folded reasoning must not repeat"
    );

    let mut empty_reasoning = ChatMessage::assistant("");
    empty_reasoning.content = None;
    empty_reasoning.reasoning_content = Some(String::new());
    let dropped = sanitize_messages(&[empty_reasoning]);
    assert!(
      dropped.is_empty(),
      "empty reasoning string is still wire-invalid"
    );
  }

  #[test]
  fn accumulator_byte_exact() {
    let mut acc = ToolCallAccumulator::default();
    acc.accumulate(&SseToolCallDelta {
      index: Some(0),
      id: Some("x".into()),
      r#type: Some("function".into()),
      function: Some(crate::types::SseFunctionDelta {
        name: Some("sh".into()),
        arguments: Some("{\"a\":".into()),
      }),
    });
    acc.accumulate(&SseToolCallDelta {
      index: Some(0),
      id: None,
      r#type: None,
      function: Some(crate::types::SseFunctionDelta {
        name: None,
        arguments: Some("1}".into()),
      }),
    });
    let calls = acc.assemble();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, "{\"a\":1}");
  }
}

#[cfg(test)]
mod accumulator_robustness_tests {
  use super::*;
  use crate::types::SseFunctionDelta;

  #[test]
  fn an_absurd_tool_call_index_cannot_size_the_accumulator() {
    let mut acc = ToolCallAccumulator::default();
    acc.accumulate(&SseToolCallDelta {
      index: Some(10_000_000),
      id: Some("a".into()),
      r#type: None,
      function: Some(SseFunctionDelta {
        name: Some("bash".into()),
        arguments: Some("{}".into()),
      }),
    });
    let calls = acc.assemble();
    assert_eq!(calls.len(), 1, "the index is bounded: {calls:?}");
    assert_eq!(calls[0].name, "bash");
  }

  #[test]
  fn parallel_calls_without_ids_still_get_distinct_ids() {
    let mut acc = ToolCallAccumulator::default();
    for (ix, name) in ["ls", "bash"].into_iter().enumerate() {
      acc.accumulate(&SseToolCallDelta {
        index: Some(ix),
        id: None,
        r#type: None,
        function: Some(SseFunctionDelta {
          name: Some(name.into()),
          arguments: Some("{}".into()),
        }),
      });
    }
    let calls = acc.assemble();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_ne!(calls[0].id, calls[1].id, "ids must be distinct: {calls:?}");
    assert!(!calls[0].id.is_empty());
  }

  /// Reads one HTTP request, hands back the body, and replies with a minimal SSE stream.
  async fn capture_request_body(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
      let (mut sock, _) = listener.accept().await.unwrap();
      let mut buf = vec![0u8; 64 * 1024];
      let n = sock.read(&mut buf).await.unwrap();
      let head = String::from_utf8_lossy(&buf[..n]).into_owned();
      let body = head
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string();
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
        reply.len(),
        reply
      );
      sock.write_all(response.as_bytes()).await.unwrap();
      body
    });
    (format!("http://{addr}"), handle)
  }

  const DONE_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

  async fn reasoning_effort_sent(thinking: Thinking) -> Option<String> {
    let (base, handle) = capture_request_body(DONE_SSE).await;
    let client = LlamaClient::new(&base);
    let _ = client
      .chat_stream(
        "m",
        &[ChatMessage::user("hi")],
        None,
        16,
        thinking,
        |_| {},
        || true,
      )
      .await;
    let body: serde_json::Value = serde_json::from_str(&handle.await.unwrap()).unwrap();
    body
      .get("reasoning_effort")
      .map(|v| v.as_str().unwrap().to_string())
  }

  #[tokio::test]
  async fn reasoning_effort_reaches_the_request_body() {
    // Auto omits the field entirely so the server keeps its own default, rather than
    // pinning the value slopfile guessed at.
    assert_eq!(reasoning_effort_sent(Thinking::Auto).await, None);
    for (t, want) in [
      (Thinking::None, "none"),
      (Thinking::Low, "low"),
      (Thinking::Medium, "medium"),
      (Thinking::High, "high"),
      (Thinking::Xhigh, "xhigh"),
    ] {
      assert_eq!(
        reasoning_effort_sent(t).await.as_deref(),
        Some(want),
        "{t:?} must serialize as {want:?}"
      );
    }
  }
}
