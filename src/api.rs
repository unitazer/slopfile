//! llama-server OpenAI-compatible client (streaming chat + health ping).

use crate::sse::{stream_chunks, SseError};
use crate::types::{
    ChatMessage, Role, SseToolCallDelta, StreamEvent, StreamResult, ToolCall, ToolDefinition,
};
use futures_util::StreamExt;
use std::error::Error;
use std::fmt;

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

/// Accumulates tool-call deltas streamed in fragments, by index, joining
/// `arguments` fragments byte-exactly.
#[derive(Debug, Clone, Default)]
pub struct ToolCallAccumulator {
    calls: Vec<SseToolCallDelta>,
}

impl ToolCallAccumulator {
    pub fn accumulate(&mut self, delta: &SseToolCallDelta) {
        let idx = delta.index.unwrap_or(0);
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

    /// Finalize into complete ToolCalls (only those with a name).
    pub fn assemble(&self) -> Vec<ToolCall> {
        self.calls
            .iter()
            .filter_map(|c| {
                let name = c.function.as_ref()?.name.clone()?;
                Some(ToolCall::new(
                    c.id.clone().unwrap_or_default(),
                    name,
                    c.function.as_ref().and_then(|f| f.arguments.clone()).unwrap_or_default(),
                ))
            })
            .collect()
    }
}

pub struct LlamaClient {
    base: String,
    model: String,
    client: reqwest::Client,
}

impl LlamaClient {
    pub fn new(api_base: &str, model: &str) -> Self {
        Self {
            base: api_base.trim_end_matches('/').to_string(),
            model: model.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// POST /v1/chat/completions, stream deltas to `on_event`, then emit
    /// `StreamEvent::Done(StreamResult)`. Returning `false` from `on_event`
    /// stops the stream early (`StreamResult::aborted` is set). `keep_going`
    /// is polled while the stream is idle (no SSE events); returning `false`
    /// aborts the stream the same way.
    pub async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        max_new_tokens: u32,
        on_event: impl FnMut(StreamEvent) -> bool,
        keep_going: impl FnMut() -> bool,
    ) -> Result<StreamResult, ApiError> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": sanitize_messages(messages),
            "stream": true,
            "max_tokens": max_new_tokens,
        });
        let mut body = body;
        if let Some(tools) = tools {
            body["tools"] = serde_json::to_value(tools).map_err(|e| {
                ApiError::Sse(SseError::Io(format!("tool serialization: {e}")))
            })?;
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

        self.consume_stream(resp.bytes_stream(), on_event, keep_going).await
    }

    /// Internal: consume an SSE byte stream (also used by tests with a fake
    /// stream). While the stream is idle, `keep_going` is polled every
    /// ~200ms; if it returns `false` the stream is aborted (server-side
    /// generation cancelled) even when no SSE events arrive.
    pub async fn consume_stream<S>(
        &self,
        raw: S,
        mut on_event: impl FnMut(StreamEvent) -> bool,
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
                    let chunk = chunk.map_err(ApiError::from)?;
                    if let Some(usage) = &chunk.usage {
                        result.usage = Some(usage.clone());
                    }
                    let mut cont = true;
                    for choice in &chunk.choices {
                        if let Some(fr) = &choice.finish_reason {
                            result.finish_reason = Some(fr.clone());
                        }
                        let d = &choice.delta;
                        if let Some(text) = &d.content {
                            result.content.push_str(text);
                            cont &= on_event(StreamEvent::Text(text.clone()));
                        }
                        if !cont {
                            break;
                        }
                        if let Some(r) = &d.reasoning_content {
                            result.reasoning.push_str(r);
                            cont &= on_event(StreamEvent::Reasoning(r.clone()));
                        }
                        if !cont {
                            break;
                        }
                        if let Some(tools) = &d.tool_calls {
                            for t in tools {
                                acc.accumulate(t);
                                cont &= on_event(StreamEvent::ToolCallFragment(t.clone()));
                                if !cont {
                                    break;
                                }
                            }
                        }
                    }
                    if !cont {
                        result.aborted = true;
                        // Explicitly cancel on the server while the slot is still
                        // busy: dropping the response alone may leave the server
                        // decoding the abandoned completion.
                        self.cancel_generation().await;
                        break;
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if !keep_going() {
                        result.aborted = true;
                        self.cancel_generation().await;
                        break;
                    }
                }
            }
        }

        result.tool_calls = acc.assemble();
        on_event(StreamEvent::Done(result.clone()));
        Ok(result)
    }

    /// Best-effort server-side cancel of the in-flight generation (`POST
    /// /abort`, the endpoint the llama.cpp WebUI stop button uses).
    ///
    /// If we merely drop the response, llama-server may keep decoding the
    /// whole abandoned completion — occupancy is only noticed between
    /// batches (`is_connection_closed` is not wired into the generation
    /// loop on many builds), which pins CPU/GPU for minutes on slow
    /// hardware and blocks the slot for the next turn. An explicit cancel
    /// makes the stop immediate and reliable. 404 on builds without the
    /// endpoint is harmless.
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

/// Drop wire-invalid messages: assistant messages with neither `content`,
/// `tool_calls` nor (non-empty) `reasoning_content` (e.g. reasoning-only
/// leftovers from a dangling turn) are rejected by llama-server with 400.
fn sanitize_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|m| match m.role {
            Role::Assistant => {
                m.content.is_some()
                    || m.reasoning_content.as_deref().is_some_and(|r| !r.is_empty())
                    || m.tool_calls.is_some()
            }
            _ => true,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Usage;

    fn fake_bytes_stream(chunks: Vec<&'static str>) -> impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> {
        let items = chunks
            .into_iter()
            .map(|s| Ok::<bytes::Bytes, reqwest::Error>(bytes::Bytes::from(s)))
            .collect::<Vec<_>>();
        futures_util::stream::iter(items)
    }

    /// A byte stream that delivers one chunk immediately and then one chunk
    /// per second (used to test the abort watchdog: while the stream stalls,
    /// the watchdog must still run).
    struct SlowBytesStream {
        items: std::vec::IntoIter<Result<bytes::Bytes, reqwest::Error>>,
        delay: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
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

    impl futures_core::Stream for SlowBytesStream {
        type Item = Result<bytes::Bytes, reqwest::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let this = &mut *self;
            if let Some(delay) = this.delay.as_mut() {
                futures_core::ready!(delay.as_mut().poll(cx));
                this.delay = None;
            }
            let Some(item) = this.items.next() else {
                return std::task::Poll::Ready(None);
            };
            this.delay =
                Some(Box::pin(tokio::time::sleep(std::time::Duration::from_millis(1000))));
            std::task::Poll::Ready(Some(item))
        }
    }

    impl Unpin for SlowBytesStream {}

    #[tokio::test]
    async fn tool_call_fragments_accumulate_end_to_end() {
        let client = LlamaClient::new("http://localhost:9", "test");
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
            .consume_stream(fake_bytes_stream(sse), |e| {
                events.push(e);
                true
            }, || true)
            .await
            .expect("stream");

        assert_eq!(result.tool_calls.len(), 1);
        let tc = &result.tool_calls[0];
        assert_eq!(tc.id, "abc123");
        assert_eq!(tc.name, "get_weather");
        assert_eq!(tc.arguments, "{\"city\":\"Paris\"}");
        assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(result.usage, Some(Usage { prompt_tokens: 10, completion_tokens: 4 }));
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ToolCallFragment(_))));
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done(_))));
    }

    #[tokio::test]
    async fn text_and_reasoning_accumulate() {
        let client = LlamaClient::new("http://localhost:9", "test");
        let sse = vec![
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" think.\",\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" there\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ];
        let result = client
            .consume_stream(fake_bytes_stream(sse), |_| true, || true)
            .await
            .expect("stream");
        assert_eq!(result.content, "Hi there");
        assert_eq!(result.reasoning, "Let me think.");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn returning_false_from_callback_aborts_the_stream() {
        let client = LlamaClient::new("http://localhost:9", "test");
        let sse = vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" two\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" three\"}}]}\n\n",
            "data: [DONE]\n\n",
        ];
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let result = client
            .consume_stream(fake_bytes_stream(sse), move |e| {
                if let StreamEvent::Text(t) = &e {
                    seen2.lock().unwrap().push(t.clone());
                }
                seen2.lock().unwrap().len() < 2
            }, || true)
            .await
            .expect("stream");
        assert!(result.aborted, "stream was aborted mid-flight");
        assert_eq!(result.content, "one two");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn watchdog_aborts_when_keep_going_returns_false() {
        let client = LlamaClient::new("http://localhost:9", "test");
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
                |_| true,
                || {
                    if stop.get() {
                        return false;
                    }
                    if t0.elapsed() >= std::time::Duration::from_millis(300) {
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

    #[test]
    fn sanitize_keeps_reasoning_only_assistant_messages() {
        let mut reasoning_only = ChatMessage::assistant("");
        reasoning_only.content = None;
        reasoning_only.reasoning_content = Some("Let me think...".to_string());
        let kept = sanitize_messages(&[reasoning_only]);
        assert_eq!(kept.len(), 1, "reasoning-only assistant reply must not be dropped");

        let mut empty_reasoning = ChatMessage::assistant("");
        empty_reasoning.content = None;
        empty_reasoning.reasoning_content = Some(String::new());
        let dropped = sanitize_messages(&[empty_reasoning]);
        assert!(dropped.is_empty(), "empty reasoning string is still wire-invalid");
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