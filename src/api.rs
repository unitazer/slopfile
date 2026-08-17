//! llama-server OpenAI-compatible client (streaming chat + health ping).

use crate::sse::{stream_chunks, SseError};
use crate::types::{
    ChatMessage, SseToolCallDelta, StreamEvent, StreamResult, ToolCall, ToolDefinition,
};
use futures_util::StreamExt;
use std::error::Error;
use std::fmt;

#[derive(Debug)]
pub enum ApiError {
    Reqwest(reqwest::Error),
    NonOkStatus(u16, String),
    Sse(SseError),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Reqwest(e) => write!(f, "http error: {e}"),
            ApiError::NonOkStatus(code, body) => write!(f, "server returned {code}: {body}"),
            ApiError::Sse(e) => write!(f, "sse error: {e}"),
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
        // Extend the vec with placeholders if needed.
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
    /// `StreamEvent::Done(StreamResult)`.
    pub async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        max_new_tokens: u32,
        on_event: impl FnMut(StreamEvent),
    ) -> Result<StreamResult, ApiError> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "max_tokens": max_new_tokens,
        });
        if let Some(tools) = tools {
            body["tools"] = serde_json::to_value(tools).map_err(|e| {
                ApiError::Sse(SseError::Io(format!("tool serialization: {e}")))
            })?;
        }

        let url = format!("{}/v1/chat/completions", self.base);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(ApiError::Reqwest)?;

        let status = resp.status();
        if !status.is_success() {
            let snippet = resp.text().await.unwrap_or_default();
            return Err(ApiError::NonOkStatus(status.as_u16(), snippet));
        }

        self.consume_stream(resp.bytes_stream(), max_new_tokens, on_event).await
    }

    /// Internal: consume an SSE byte stream (also used by tests with a fake
    /// stream).
    pub async fn consume_stream<S, F>(
        &self,
        raw: S,
        _max_new_tokens: u32,
        mut on_event: F,
    ) -> Result<StreamResult, ApiError>
    where
        S: futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
        F: FnMut(StreamEvent),
    {
        let mut result = StreamResult::default();
        let mut acc = ToolCallAccumulator::default();
        let mut chunk_stream = stream_chunks(raw);

        while let Some(chunk) = chunk_stream.next().await {
            let chunk = chunk.map_err(ApiError::from)?;
            if let Some(usage) = &chunk.usage {
                result.usage = Some(usage.clone());
            }
            if let Some(timings) = &chunk.timings {
                result.timings = Some(timings.clone());
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

        result.tool_calls = acc.assemble();
        on_event(StreamEvent::Done(result.clone()));
        Ok(result)
    }

    /// Simple health ping: GET {base}/health → 200.
    pub async fn health(&self) -> bool {
        let url = format!("{}/health", self.base);
        self.client.get(&url).send().await.map(|r| r.status().is_success()).unwrap_or(false)
    }
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
            .consume_stream(fake_bytes_stream(sse), 4096, |e| events.push(e))
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
            .consume_stream(fake_bytes_stream(sse), 4096, |_| {})
            .await
            .expect("stream");
        assert_eq!(result.content, "Hi there");
        assert_eq!(result.reasoning, "Let me think.");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
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