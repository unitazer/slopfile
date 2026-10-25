use crate::sse::{SseError, stream_chunks};
use crate::types::{
  AppConfig, ChatMessage, Endpoint, ReasoningStyle, Role, SseToolCallDelta, StreamEvent,
  StreamResult, Thinking, ToolCall, ToolDefinition,
};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::time::Duration;

const KEEP_GOING_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum ApiError {
  Reqwest(reqwest::Error),
  /// `retry_after` is the server saying "come back in a moment". A limit that clears in
  /// five seconds must not park a session until a human notices.
  NonOkStatus(u16, String, bool),
  Sse(SseError),
  Timeout(std::time::Duration),
}

impl fmt::Display for ApiError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ApiError::Reqwest(e) => write!(f, "http error: {e}"),
      // Providers explain themselves in `error.message`; the rest is metadata and headers. A raw
      // body here would bury the one sentence that says what to do about it.
      ApiError::NonOkStatus(code, body, _) => {
        write!(
          f,
          "the provider refused the request ({code}): {}",
          provider_message(body)
        )
      }
      ApiError::Sse(e) => write!(f, "sse error: {e}"),
      ApiError::Timeout(d) => write!(f, "request timed out after {d:?}"),
    }
  }
}

/// Pull the human half out of a provider's error body: OpenRouter and OpenAI both nest the sentence
/// under `error.message`. Falls back to the body itself, trimmed, so an unfamiliar shape still
/// says something.
fn provider_message(body: &str) -> String {
  let trimmed = body.trim();
  if trimmed.is_empty() {
    return "no detail returned".to_string();
  }
  if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
    let paths: [&[&str]; 3] = [&["error", "message"], &["message"], &["error"]];
    for path in paths {
      let mut cur = &v;
      let mut ok = true;
      for key in path {
        match cur.get(key) {
          Some(next) => cur = next,
          None => {
            ok = false;
            break;
          }
        }
      }
      if ok
        && let Some(text) = cur.as_str()
        && !text.trim().is_empty()
      {
        // Capped here as well as on the fallback paths: this string is written into the
        // transcript and resent on every later turn, and a gateway that echoes the offending
        // request back inside `error.message` would otherwise paste a whole payload into the
        // user's file.
        return text.trim().chars().take(300).collect();
      }
    }
    return trimmed.chars().take(300).collect();
  }
  trimmed.chars().take(300).collect()
}

impl Error for ApiError {}

impl ApiError {
  /// Is this worth retrying five seconds from now?
  ///
  /// A 4xx is the provider saying no: a bad key, an unknown model, a request it will never accept.
  /// Retrying burns whatever budget produced the refusal, and a daily quota does not come back in
  /// five seconds. Transport errors and 5xx are the opposite -- the request never got a real
  /// answer, so the existing retry-and-park path is the right response.
  pub fn is_terminal(&self) -> bool {
    let ApiError::NonOkStatus(code, _, retry_after) = self else {
      return false;
    };
    if !(400..500).contains(code) {
      return false;
    }
    // Not every 4xx is a decision about the request. 408 and 425 are the server saying "not right
    // now", and a gateway in front of a flaky backend reports those transient upstream failures
    // as a 4xx of their own. Parking on one of those turns a hiccup into a session that stops
    // until a human intervenes -- the opposite failure from the one this guards against, and much
    // more confusing. An explicit `Retry-After` means the same thing.
    if matches!(code, 408 | 425) || *retry_after {
      return false;
    }
    true
  }

  /// Did the provider reject us for sending too much, rather than for anything about the request's
  /// shape?
  ///
  /// Worth separating because the advice is the opposite. A rate limit clears itself; an oversized
  /// prompt fails identically forever until the transcript shrinks. This is reachable on the very
  /// first turn against a hosted endpoint, where there is no measured sample to size the request
  /// with and so nothing prunes it -- we do not guess at token counts, which is the right call on
  /// its own but leaves the ceiling unenforced until the provider names it.
  pub fn is_context_overflow(&self) -> bool {
    let ApiError::NonOkStatus(code, body, _) = self else {
      return false;
    };
    // 400 is what OpenAI-compatible endpoints use, 413 is the generic "too large", and several
    // llama builds answer 500 for a context-size rejection. The status alone is not reliable
    // enough to rule the case out, so read the message and decide on that.
    if *code != 400 && *code != 413 && *code != 500 {
      return false;
    }
    let m = provider_message(body).to_ascii_lowercase();
    [
      "context length",
      "context_length",
      "maximum context",
      "too many tokens",
      "prompt is too long",
      "reduce the length",
      "input is too long",
      "request too large",
      "exceeds the context",
      "n_ctx",
      "exceeds the model",
      // A reverse proxy (nginx is the common one in front of a hosted provider) answers 413 with
      // an HTML page, not a JSON sentence, so the standard reason phrase has to match too.
      "entity too large",
      "payload too large",
      "request entity too large",
    ]
    .iter()
    .any(|needle| m.contains(needle))
  }
}

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
        function: None,
      });
    }
    let slot = &mut self.calls[idx];
    if let Some(id) = &delta.id {
      slot.id = Some(id.clone());
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
  endpoint: Endpoint,
  headers: reqwest::header::HeaderMap,
  client: reqwest::Client,
}

impl LlamaClient {
  /// The app-wide default endpoint. A broken `provider = "..."` in the config falls back to
  /// `api_base` here rather than refusing to start: the daemon sweeps every transcript, and one
  /// unusable default must not take the local session down with it. Sessions that name a provider
  /// go through `for_provider`, which does report the error.
  pub fn new(cfg: &AppConfig) -> Self {
    // The sweep builds one of these per file per tick, so this resolves the endpoint *quietly*:
    // a misconfigured app-level provider would otherwise log several times a second, per session,
    // and bury the one line that explains it. `main` logs the resolved endpoint once at startup,
    // and a per-session `sf send` still reports the real error against the named provider.
    let endpoint = cfg
      .endpoint(None, &BTreeMap::new())
      .unwrap_or_else(|_| Endpoint::local(&cfg.api_base));
    Self::with_endpoint(endpoint, reqwest::Client::new())
  }

  fn with_endpoint(endpoint: Endpoint, client: reqwest::Client) -> Self {
    // Build the map once rather than per request: a bad header name is a config error, and
    // catching it here means one clear message instead of a 431 on every turn.
    let headers = endpoint
      .headers
      .iter()
      .filter_map(|(name, value)| {
        match (
          reqwest::header::HeaderName::from_bytes(name.as_bytes()),
          reqwest::header::HeaderValue::from_str(value),
        ) {
          (Ok(n), Ok(v)) => Some((n, v)),
          _ => {
            tracing::warn!(
              "ignoring malformed header `{name}` for provider base {}",
              endpoint.base
            );
            None
          }
        }
      })
      .collect();
    Self {
      endpoint,
      headers,
      client,
    }
  }

  /// The same client -- and the same connection pool -- pointed at a different endpoint.
  pub fn for_provider(
    &self,
    cfg: &AppConfig,
    name: Option<&str>,
    extra_headers: &std::collections::BTreeMap<String, String>,
  ) -> Result<Self, String> {
    let endpoint = cfg.endpoint(name, extra_headers)?;
    tracing::debug!(
      "provider {:?} → {} ({} header(s), llama={})",
      name,
      endpoint.base,
      endpoint.headers.len(),
      endpoint.llama
    );
    Ok(Self::with_endpoint(endpoint, self.client.clone()))
  }

  pub fn endpoint(&self) -> &Endpoint {
    &self.endpoint
  }

  pub(crate) fn request(&self, url: &str) -> reqwest::RequestBuilder {
    self.client.post(url).headers(self.headers.clone())
  }

  pub async fn ensure_model_loaded(&self, model: &str) {
    // Preloading is llama.cpp's own convention. Any other endpoint has no such route, and asking
    // costs a doomed round trip (and a warning) on every single turn.
    if !self.endpoint.llama {
      return;
    }
    let body = serde_json::json!({ "model": model });
    let mut last_err = String::new();
    for path in ["models/load", "v1/models/load"] {
      let url = self.endpoint.raw_url(path);
      tracing::debug!("sending request {body:#?} to {url}");
      match self.request(&url).json(&body).send().await {
        Ok(resp) => {
          let status = resp.status();
          tracing::debug!("response from {url}: status:{status:?}");
          if status.is_success() {
            return;
          }
          last_err = format!("{}: HTTP {}", url, status);
        }
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
        "messages": sanitize_messages(messages, self.endpoint.llama),
        "stream": true,
        "max_tokens": max_new_tokens,
    });
    let mut body = body;
    if let Some(tools) = tools {
      body["tools"] = serde_json::to_value(tools)
        .map_err(|e| ApiError::Sse(SseError::Io(format!("tool serialization: {e}"))))?;
    }
    // Omitted for `Auto` so the server keeps its own default. Sent otherwise, in whichever shape
    // the provider reads: llama-server forwards `reasoning_effort` to the chat template (and maps
    // `none` to enable_thinking=false), OpenAI wants that name, and OpenRouter's unified
    // interface is a `reasoning` object. A model can advertise one spelling and not the other.
    if let Some(effort) = thinking.wire_value() {
      match self.endpoint.reasoning_style {
        ReasoningStyle::Effort => body["reasoning_effort"] = serde_json::Value::from(effort),
        ReasoningStyle::Object => {
          body["reasoning"] = serde_json::json!({ "effort": effort });
        }
      }
    }
    // OpenRouter reports usage unprompted; OpenAI only does so when asked. Without it the whole
    // calibration in `tokens` has nothing to measure against and the turn stays uncounted. Scoped
    // to non-llama so the local llama-server request goes out byte-identical to before.
    if !self.endpoint.llama {
      body["stream_options"] = serde_json::json!({ "include_usage": true });
    }

    let url = self.endpoint.url("chat/completions");
    tracing::debug!("sending request {body:#?} to {url}");
    let resp = tokio::time::timeout(
      std::time::Duration::from_secs(60),
      self.request(&url).json(&body).send(),
    )
    .await
    .map_err(|_| ApiError::Timeout(std::time::Duration::from_secs(60)))?
    .map_err(ApiError::Reqwest)?;

    let status = resp.status();
    if !status.is_success() {
      // Read before the body: the response is still live here, and `Retry-After` is a header --
      // the one signal that separates "come back in five seconds" from "never", and the body
      // almost never carries it.
      let retry_after = resp.headers().contains_key(reqwest::header::RETRY_AFTER);
      let snippet = resp.text().await.unwrap_or_default();
      tracing::warn!("request failed: status:{status:?} snippet:{snippet}");
      return Err(ApiError::NonOkStatus(status.as_u16(), snippet, retry_after));
    }
    tracing::debug!("response from {url}: status:{status:?}");

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
              tracing::debug!("response chunk: {chunk:?}");
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
                  // llama.cpp calls it `reasoning_content`, OpenAI and OpenRouter call it
                  // `reasoning`. Same thing, so take whichever arrived.
                  if let Some(r) = d.reasoning_content.as_ref().or(d.reasoning.as_ref()) {
                      on_event(StreamEvent::Reasoning(r.clone()));
                  }
                  if let Some(tools) = &d.tool_calls {
                      for t in tools {
                          acc.accumulate(t);
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
          }
      }
    }

    result.tool_calls = acc.assemble();
    Ok(result)
  }

  pub async fn cancel_generation(&self) {
    // Dropping the response stream is the whole of abort on an OpenAI-compatible endpoint; only
    // llama-server needs telling, because it keeps generating into a slot until asked not to.
    if !self.endpoint.llama {
      tracing::debug!(
        "cancel_generation: dropping the stream for {}",
        self.endpoint.base
      );
      return;
    }
    let url = self.endpoint.raw_url("abort");
    tracing::debug!("sending request to {url}");
    match self
      .request(&url)
      .timeout(std::time::Duration::from_millis(2000))
      .send()
      .await
    {
      Ok(resp) => tracing::debug!("response from {url}: status:{:?}", resp.status()),
      Err(e) => tracing::warn!("cancel_generation: {url}: {e}"),
    }
  }
}

/// `keep_reasoning` is llama.cpp's convention; anything else rejects the field outright
/// ("Unrecognized request argument supplied: reasoning_content"), so it is stripped there and
/// kept here. Either way a reasoning-only assistant turn is folded into `content`, because a turn
/// with neither content nor tool calls is not a turn.
pub(crate) fn sanitize_messages(
  messages: &[ChatMessage],
  keep_reasoning: bool,
) -> Vec<ChatMessage> {
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
      let mut m2 = m.clone();
      // Fold first: an assistant turn carrying only reasoning still has to reach the model as
      // content, and stripping the reasoning before looking at it would leave nothing behind.
      if m2.role == Role::Assistant
        && m2.content.is_none()
        && m2.tool_calls.is_none()
        && let Some(r) = m2.reasoning_content.as_deref().filter(|r| !r.is_empty())
      {
        m2.content = Some(r.to_string());
        m2.reasoning_content = None;
      }
      if !keep_reasoning {
        m2.reasoning_content = None;
      }
      m2
    })
    .collect()
}

/// A client pointed at a base, for tests that never need a real endpoint behind it.
#[cfg(test)]
fn client_at(base: &str) -> LlamaClient {
  LlamaClient::with_endpoint(Endpoint::local(base), reqwest::Client::new())
}

/// `http://localhost:9` is the discard port: connecting fails immediately rather than hanging.
#[cfg(test)]
fn dead_client() -> LlamaClient {
  client_at("http://localhost:9")
}

/// Same client shape as production, so a test exercising header or request construction takes the
/// same path the daemon does.
#[cfg(test)]
pub(crate) fn test_client_at(base: String) -> LlamaClient {
  LlamaClient::with_endpoint(Endpoint::local(&base), reqwest::Client::new())
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
    let client = dead_client();
    let sse = vec![
      "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"abc123\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"cit\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"y\\\":\\\"Pa\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ris\\\"}\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\n",
      "data: [DONE]\n\n",
    ];
    let result = client
      .consume_stream(fake_bytes_stream(sse), |_| {}, || true)
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
        completion_tokens: 4,
        completion_tokens_details: None
      })
    );
  }

  #[tokio::test]
  async fn text_and_reasoning_accumulate() {
    let client = dead_client();
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
    assert_eq!(result.finish_reason.as_deref(), Some("stop"));
  }

  /// Parking is the harsh failure: the session stops until a human intervenes. Only park on a 4xx
  /// that is actually a decision about the request.
  #[test]
  fn a_4xx_that_only_means_not_right_now_is_not_terminal() {
    for code in [408u16, 425] {
      let e = ApiError::NonOkStatus(
        code,
        r#"{"error":{"message":"upstream not ready"}}"#.to_string(),
        false,
      );
      assert!(
        !e.is_terminal(),
        "{code} means 'not right now', so it belongs on the retry-and-park path"
      );
    }
    // The one that matters most: a rate limit that says when it lifts. OpenRouter answers 429 and
    // `Retry-After` as a *header*, so reading it from the body finds nothing and the session parks
    // for a limit that clears in seconds.
    let polite = ApiError::NonOkStatus(
      429,
      r#"{"error":{"message":"Rate limit exceeded"}}"#.to_string(),
      true,
    );
    assert!(
      !polite.is_terminal(),
      "a Retry-After header means come back, not stop until a human intervenes"
    );

    // The decisions that really are permanent.
    for code in [400u16, 401, 402, 404, 429] {
      assert!(
        ApiError::NonOkStatus(code, r#"{"error":{"message":"no"}}"#.to_string(), false)
          .is_terminal(),
        "{code} is a decision about the request"
      );
    }
  }

  /// A gateway that sends `reasoning` as a content-block array must not cost us the whole answer.
  /// Reading it as a typed String fails deserialization of the entire chunk, which aborts the
  /// stream mid-generation and discards the text that had already arrived -- the exact failure
  /// here that matters most. The unfamiliar shape is dropped, as it was before the field existed.
  #[tokio::test]
  async fn an_unexpected_reasoning_shape_does_not_kill_the_stream() {
    let sse = concat!(
      r#"data: {"choices":[{"index":0,"delta":{"content":"partial answer","reasoning":{"content":"thinking"}}}]}"#,
      "\n\n",
      r#"data: {"choices":[{"index":0,"delta":{"content":" and more"},"finish_reason":null}]}"#,
      "\n\n",
      r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
      "\n\n",
      "data: [DONE]\n\n"
    );
    let result = dead_client()
      .consume_stream(fake_bytes_stream(vec![sse]), |_| {}, || true)
      .await
      .expect("an odd reasoning shape must not abort the stream");
    assert_eq!(
      result.content, "partial answer and more",
      "text streamed before and after the odd field must both survive"
    );
  }

  #[tokio::test]
  async fn keep_going_returning_false_aborts_the_stream() {
    let client = dead_client();
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
    let client = dead_client();
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
  async fn a_stalled_stream_is_not_timed_out() {
    let client = dead_client();
    let sse = vec!["data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n"];
    let t0 = std::time::Instant::now();
    let result = client
      .consume_stream(
        SlowBytesStream::new(sse),
        |_| {},
        || t0.elapsed().as_millis() < 400,
      )
      .await
      .expect("stream");
    assert!(
      result.aborted,
      "keep_going should still be the only way out"
    );
    assert_eq!(result.content, "one");
    assert!(
      t0.elapsed() < std::time::Duration::from_millis(900),
      "the abort check must not wait for another chunk"
    );
  }

  #[test]
  fn sanitize_keeps_reasoning_only_assistant_messages() {
    let mut reasoning_only = ChatMessage::assistant("");
    reasoning_only.content = None;
    reasoning_only.reasoning_content = Some("Let me think...".to_string());
    let kept = sanitize_messages(&[reasoning_only], true);
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
    let dropped = sanitize_messages(&[empty_reasoning], true);
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
      function: Some(crate::types::SseFunctionDelta {
        name: Some("sh".into()),
        arguments: Some("{\"a\":".into()),
      }),
    });
    acc.accumulate(&SseToolCallDelta {
      index: Some(0),
      id: None,
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

  /// The header has to actually be read off the response, not guessed from the body -- OpenRouter
  /// answers 429 with `Retry-After` and almost nothing else useful, and reading the wrong one
  /// parks the session for a limit that clears in seconds.
  #[tokio::test]
  async fn a_retry_after_header_is_read_off_the_response() {
    let (base, handle) = capture_response(
      429,
      "Retry-After",
      "5",
      r#"{"error":{"message":"Rate limit exceeded"}}"#,
    )
    .await;
    let client = client_at(&base);
    let err = client
      .chat_stream(
        "m",
        &[ChatMessage::user("hi")],
        None,
        100,
        crate::types::Thinking::Auto,
        |_| {},
        || true,
      )
      .await
      .expect_err("429 is an error");
    let _ = handle.await;

    assert!(
      !err.is_terminal(),
      "a limit that lifts in five seconds must not park a session: {err}"
    );
  }

  /// the only symptom is a warning -- which quietly switches off budget enforcement for good.
  #[tokio::test]
  async fn the_tokenizer_probe_sends_the_providers_headers() {
    let (base, handle) = capture_request(r#"data: {"choices":[]}"#).await;
    let endpoint = Endpoint {
      base,
      headers: [("Authorization".to_string(), "Bearer sk-secret".to_string())]
        .into_iter()
        .collect(),
      llama: true,
      reasoning_style: ReasoningStyle::Effort,
    };
    let client = LlamaClient::with_endpoint(endpoint.clone(), reqwest::Client::new());
    let _ =
      crate::tokens::request_token_count(&endpoint, &client, "m", &[ChatMessage::user("hi")]).await;

    let head = handle.await.unwrap();
    assert!(
      head
        .to_ascii_lowercase()
        .contains("authorization: bearer sk-secret"),
      "the probe must authenticate like every other route:\n{head}"
    );
  }

  /// Reads one HTTP request, hands back the whole thing, and replies with a minimal SSE stream.
  /// The head matters as much as the body here: headers are how a provider gets its key.
  /// A non-2xx reply with a chosen extra header, so error paths can be exercised against a real
  /// response rather than a hand-built error value.
  async fn capture_response(
    status: u16,
    header: &'static str,
    value: &'static str,
    body: &'static str,
  ) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
      let (mut sock, _) = listener.accept().await.unwrap();
      let mut buf = vec![0u8; 64 * 1024];
      let n = sock.read(&mut buf).await.unwrap();
      let head = String::from_utf8_lossy(&buf[..n]).into_owned();
      let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{header}: {value}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
      );
      sock.write_all(response.as_bytes()).await.unwrap();
      head
    });
    (format!("http://{addr}"), handle)
  }

  async fn capture_request(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
      let (mut sock, _) = listener.accept().await.unwrap();
      let mut buf = vec![0u8; 64 * 1024];
      let n = sock.read(&mut buf).await.unwrap();
      let head = String::from_utf8_lossy(&buf[..n]).into_owned();
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
        reply.len(),
        reply
      );
      sock.write_all(response.as_bytes()).await.unwrap();
      head
    });
    (format!("http://{addr}"), handle)
  }

  async fn capture_request_body(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    let (base, handle) = capture_request(reply).await;
    let base2 = base.clone();
    let body_only = tokio::spawn(async move {
      handle
        .await
        .unwrap()
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string()
    });
    (base2, body_only)
  }

  const DONE_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

  async fn reasoning_effort_sent(thinking: Thinking) -> Option<String> {
    let (base, handle) = capture_request_body(DONE_SSE).await;
    let client = client_at(&base);
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

  /// Drives one request against a capture server and hands back the request head and body.
  async fn round_trip(
    endpoint: Endpoint,
    messages: Vec<ChatMessage>,
  ) -> (String, serde_json::Value) {
    round_trip_thinking(endpoint, messages, Thinking::Auto).await
  }

  async fn round_trip_thinking(
    endpoint: Endpoint,
    messages: Vec<ChatMessage>,
    thinking: Thinking,
  ) -> (String, serde_json::Value) {
    let (base, handle) = capture_request(DONE_SSE).await;
    let client = LlamaClient::with_endpoint(Endpoint { base, ..endpoint }, reqwest::Client::new());
    let _ = client
      .chat_stream("m", &messages, None, 16, thinking, |_| {}, || true)
      .await;
    let raw = handle.await.unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").expect("a request head");
    (
      head.to_string(),
      serde_json::from_str(body).expect("a json body"),
    )
  }

  fn hosted(base: &str) -> Endpoint {
    Endpoint {
      base: base.to_string(),
      reasoning_style: ReasoningStyle::Effort,
      headers: [(
        "Authorization".to_string(),
        "Bearer sk-or-v1-test".to_string(),
      )]
      .into_iter()
      .collect(),
      llama: false,
    }
  }

  fn assistant_with_reasoning() -> ChatMessage {
    let mut m = ChatMessage::assistant("");
    m.content = Some("the answer".to_string());
    m.reasoning_content = Some("the thinking".to_string());
    m
  }

  #[tokio::test]
  async fn provider_headers_reach_the_wire() {
    let (head, _) = round_trip(hosted("http://x"), vec![ChatMessage::user("hi")]).await;
    assert!(
      head.contains("authorization: Bearer sk-or-v1-test"),
      "the API key must be sent on every request: {head}"
    );
  }

  /// OpenRouter's unified interface is a `reasoning` object; OpenAI's is `reasoning_effort`. A
  /// model can advertise one and not the other, so the shape has to follow the provider.
  #[tokio::test]
  async fn the_thinking_knob_takes_the_shape_the_provider_reads() {
    for (style, want_object) in [
      (ReasoningStyle::Object, true),
      (ReasoningStyle::Effort, false),
    ] {
      let (_, body) = round_trip_thinking(
        Endpoint {
          reasoning_style: style,
          ..hosted("http://x")
        },
        vec![ChatMessage::user("hi")],
        Thinking::High,
      )
      .await;
      assert_eq!(
        body.get("reasoning").is_some(),
        want_object,
        "{style:?} and the reasoning object: {body}"
      );
      assert_eq!(
        body.get("reasoning_effort").is_some(),
        !want_object,
        "{style:?} and reasoning_effort: {body}"
      );
      if want_object {
        assert_eq!(body["reasoning"]["effort"], "high");
      }
    }
  }

  #[tokio::test]
  async fn every_effort_level_maps_onto_the_reasoning_object() {
    // The names OpenRouter documents are exactly the ones `Thinking` already speaks.
    for (thinking, effort) in [
      (Thinking::None, "none"),
      (Thinking::Low, "low"),
      (Thinking::Medium, "medium"),
      (Thinking::High, "high"),
      (Thinking::Xhigh, "xhigh"),
    ] {
      let (_, body) = round_trip_thinking(
        Endpoint {
          reasoning_style: ReasoningStyle::Object,
          ..hosted("http://x")
        },
        vec![ChatMessage::user("hi")],
        thinking,
      )
      .await;
      assert_eq!(body["reasoning"]["effort"], effort, "{thinking:?}: {body}");
    }
  }

  /// OpenRouter reports usage unprompted but OpenAI does not, and the calibration has nothing to
  /// measure against without it. The llama-server request must not change shape.
  /// The whole chain for a turn that stopped because it spent its output allowance thinking: the
  /// usage block arrives on the last SSE chunk, and the diagnosis has to come out "reasoning",
  /// not "context", or the model is told to prune a transcript that was never the problem.
  #[tokio::test]
  async fn a_length_stop_from_thinking_is_distinguishable_on_the_stream_alone() {
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![Ok(bytes::Bytes::from(
      "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":500,\"completion_tokens\":4000,\"completion_tokens_details\":{\"reasoning_tokens\":3990}}}\n\ndata: [DONE]\n\n",
    ))];
    let result = dead_client()
      .consume_stream(futures_util::stream::iter(chunks), |_| {}, || true)
      .await
      .unwrap();
    assert_eq!(result.finish_reason.as_deref(), Some("length"));
    assert!(
      result
        .usage
        .as_ref()
        .unwrap()
        .truncated_by_reasoning(&result.content),
      "nothing visible was written and thinking took the whole allowance"
    );

    // The same stop, with a reply actually written, is a different problem.
    let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = vec![Ok(bytes::Bytes::from(
      "data: {\"choices\":[{\"delta\":{\"content\":\"partial answer\"},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":500,\"completion_tokens\":4000,\"completion_tokens_details\":{\"reasoning_tokens\":3990}}}\n\ndata: [DONE]\n\n",
    ))];
    let result = dead_client()
      .consume_stream(futures_util::stream::iter(chunks), |_| {}, || true)
      .await
      .unwrap();
    assert!(
      !result
        .usage
        .as_ref()
        .unwrap()
        .truncated_by_reasoning(&result.content),
      "it wrote something before running out"
    );
  }

  #[test]
  fn a_provider_refusal_reads_as_its_own_sentence_not_a_json_dump() {
    let body = r#"{"error":{"message":"Rate limit exceeded: free-models-per-day. Add 10 credits to unlock 1000 free model requests per day","code":429,"metadata":{"headers":{"X-RateLimit-Limit":"50"}}}}"#;
    let err = ApiError::NonOkStatus(429, body.to_string(), false);
    let shown = err.to_string();
    assert!(
      shown.contains("Rate limit exceeded: free-models-per-day"),
      "{shown}"
    );
    assert!(
      !shown.contains("X-RateLimit") && !shown.contains("\"code\""),
      "headers and codes are metadata, not the explanation: {shown}"
    );
    // An unfamiliar or empty shape still says something rather than rendering blank.
    assert!(
      ApiError::NonOkStatus(500, String::new(), false)
        .to_string()
        .contains("no detail returned")
    );
    assert!(
      ApiError::NonOkStatus(502, "upstream exploded".to_string(), false)
        .to_string()
        .contains("upstream exploded")
    );
  }

  /// An oversized prompt fails forever until the transcript shrinks, which is the opposite of a
  /// rate limit -- so the two must not share advice.
  #[test]
  fn an_oversized_prompt_is_told_apart_from_a_rate_limit() {
    let overflow = ApiError::NonOkStatus(
      400,
      r#"{"error":{"message":"This model's maximum context length is 512000 tokens, however you requested 600123 tokens"}}"#
        .to_string(),
      false,
    );
    assert!(overflow.is_terminal(), "a 400 is still terminal");
    assert!(
      overflow.is_context_overflow(),
      "otherwise the user is told to re-send an identical request forever"
    );

    // A rate limit is terminal but not an overflow, and must not claim the transcript is the problem.
    let limited = ApiError::NonOkStatus(
      429,
      r#"{"error":{"message":"Rate limit exceeded: free-models-per-day"}}"#.to_string(),
      false,
    );
    assert!(limited.is_terminal());
    assert!(!limited.is_context_overflow());

    // Several llama builds answer 500 for a context-size rejection, and 413 is the generic
    // "too large", so the message decides rather than the status alone.
    for code in [500u16, 413] {
      assert!(
        ApiError::NonOkStatus(code, "maximum context length exceeded".to_string(), false)
          .is_context_overflow(),
        "{code} with a size complaint is still a size complaint"
      );
    }
    // A 500 that is not about size stays a transient failure.
    assert!(
      !ApiError::NonOkStatus(500, "upstream exploded".to_string(), false).is_context_overflow()
    );
  }

  /// A refusal is the provider saying no. Retrying it spends more of the budget that produced the
  /// refusal, and a daily quota does not come back in five seconds.
  #[test]
  fn only_a_refusal_is_terminal() {
    assert!(
      ApiError::NonOkStatus(429, String::new(), false).is_terminal(),
      "rate limited"
    );
    assert!(
      ApiError::NonOkStatus(401, String::new(), false).is_terminal(),
      "bad key"
    );
    assert!(
      ApiError::NonOkStatus(404, String::new(), false).is_terminal(),
      "no such model"
    );
    assert!(
      !ApiError::NonOkStatus(500, String::new(), false).is_terminal(),
      "upstream blip"
    );
    assert!(!ApiError::NonOkStatus(503, String::new(), false).is_terminal());
    assert!(
      !ApiError::Timeout(std::time::Duration::from_secs(60)).is_terminal(),
      "transport trouble is worth another try"
    );
  }

  #[tokio::test]
  async fn usage_is_asked_for_on_hosted_endpoints_only() {
    for (endpoint, wanted) in [
      (hosted("http://x"), true),
      (Endpoint::local("http://x"), false),
    ] {
      let (_, body) = round_trip(endpoint, vec![ChatMessage::user("hi")]).await;
      assert_eq!(
        body.get("stream_options").is_some(),
        wanted,
        "stream_options present={wanted}: {body}"
      );
    }
  }

  /// The whole point of putting the key in a transcript: config supplies the endpoint, the fence
  /// supplies the credential, and both have to arrive as real HTTP headers.
  #[tokio::test]
  async fn a_fence_header_overrides_the_providers_and_reaches_the_wire() {
    let (base, handle) = capture_request(DONE_SSE).await;
    let mut cfg = AppConfig::default();
    cfg.providers.insert(
      "openrouter".to_string(),
      crate::types::Provider {
        base_url: format!("{base}/api/v1"),
        reasoning_style: ReasoningStyle::Effort,
        headers: [(
          "Authorization".to_string(),
          "Bearer from-config".to_string(),
        )]
        .into_iter()
        .collect(),
        llama: false,
      },
    );
    let client = dead_client()
      .for_provider(
        &cfg,
        Some("openrouter"),
        &[("Authorization".to_string(), "sk-or-v1-inline".to_string())]
          .into_iter()
          .collect(),
      )
      .unwrap();
    let _ = client
      .chat_stream(
        "m",
        &[ChatMessage::user("hi")],
        None,
        16,
        Thinking::Auto,
        |_| {},
        || true,
      )
      .await;

    let raw = handle.await.unwrap();
    assert!(raw.contains("authorization: sk-or-v1-inline"), "{raw}");
    assert!(!raw.contains("from-config"), "the fence must win: {raw}");
  }

  #[tokio::test]
  async fn a_hosted_endpoint_rejects_reasoning_content_but_llama_server_wants_it() {
    let (_, body) = round_trip(hosted("http://x"), vec![assistant_with_reasoning()]).await;
    assert!(
      body["messages"][0].get("reasoning_content").is_none(),
      "OpenAI-compatible endpoints reject the field outright: {body}"
    );

    let (_, body) = round_trip(
      Endpoint::local("http://x"),
      vec![assistant_with_reasoning()],
    )
    .await;
    assert_eq!(
      body["messages"][0]["reasoning_content"], "the thinking",
      "llama.cpp reads its own field: {body}"
    );
  }

  #[tokio::test]
  async fn a_reasoning_only_reply_still_has_content_for_a_hosted_endpoint() {
    let mut m = ChatMessage::assistant("");
    m.content = None;
    m.reasoning_content = Some("the thinking".to_string());
    let (_, body) = round_trip(hosted("http://x"), vec![m]).await;
    assert_eq!(
      body["messages"][0]["content"], "the thinking",
      "an assistant turn with neither content nor tool calls is not a turn: {body}"
    );
  }

  #[tokio::test]
  async fn the_base_url_is_asked_at_the_right_path() {
    for (configured, want) in [
      ("https://openrouter.ai/api", "/v1/chat/completions"),
      ("https://openrouter.ai/api/v1", "/v1/chat/completions"),
      ("https://api.openai.com/v1/", "/v1/chat/completions"),
    ] {
      let (head, _) = round_trip(hosted(configured), vec![ChatMessage::user("hi")]).await;
      assert!(
        head.contains(want),
        "{configured} should be asked at {want}: {head}"
      );
      assert!(
        !head.contains("/v1/v1/"),
        "{configured} must not double the version: {head}"
      );
    }
  }

  #[tokio::test]
  async fn a_reasoning_stream_is_read_under_either_name() {
    // llama.cpp says `reasoning_content`, OpenAI and OpenRouter say `reasoning`. Reading only one
    // silently loses the thinking on the other.
    for (field, sse) in [
      (
        "reasoning_content",
        r#"data: {"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#,
      ),
      (
        "reasoning",
        r#"data: {"choices":[{"delta":{"reasoning":"thinking"}}]}"#,
      ),
    ] {
      let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> =
        vec![Ok(bytes::Bytes::from(format!("{sse}\n\ndata: [DONE]\n\n")))];
      let mut seen = String::new();
      let client = dead_client();
      let _out = client
        .consume_stream(
          futures_util::stream::iter(chunks),
          |ev| {
            if let StreamEvent::Reasoning(r) = ev {
              seen.push_str(&r);
            }
          },
          || true,
        )
        .await
        .unwrap();
      assert_eq!(seen, "thinking", "from the `{field}` field");
    }
  }
}
