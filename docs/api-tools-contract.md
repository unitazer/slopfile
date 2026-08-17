# api + sse + tools contracts (for subagents)

Read `PLAN.md` first. This file nails down the public API of `src/sse.rs`,
`src/api.rs` and `src/tools.rs` so the three modules compose.

## src/sse.rs — SSE parser (pure, no I/O)

Parses the streaming response of `/v1/chat/completions` (OpenAI SSE format:
lines `data: <json>`, terminated by `data: [DONE]`).

```rust
/// Consume an async chunks stream and yield parsed chunks.
pub async fn stream_chunks<S>(stream: S) -> StreamChunkStream
// where S: Stream<Item = Result<Bytes, reqwest::Error>>
// Implemented using futures-util + bytes (already deps).
```

`StreamChunkStream` is a `futures_core::Stream<Item = Result<SseChunk, SseError>>`.

- Ignore `data: [DONE]`, ignore empty/comment lines, ignore lines not
  starting with `data:`.
- Each `data:` payload must deserialize into `types::SseChunk`.
- Errors: malformed JSON → `SseError::BadJson(String)`.
- Provide a unit test that feeds a canned SSE text fixture (as bytes) and
  asserts the parsed chunks (verify `types::SseChunk` fields match). Do not
  touch main.rs/types.rs.

## src/api.rs — llama-server client

Turns messages into a streaming request, aggregates the stream, returns
`types::StreamResult` + emits `types::StreamEvent`.

```rust
pub struct LlamaClient { base: String, model: String, client: reqwest::Client }

impl LlamaClient {
    pub fn new(api_base: &str, model: &str) -> Self;

    /// POST /v1/chat/completions with stream=true. `tools` is the tool list
    /// (Some => include in request). `on_event` is called for each
    /// `StreamEvent` as it arrives (Text / Reasoning / ToolCallFragment),
    /// then a final `StreamEvent::Done(StreamResult)`.
    pub async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
        max_new_tokens: u32,
        on_event: impl FnMut(StreamEvent),
    ) -> Result<StreamResult, ApiError>;

    /// Simple health ping: GET {base}/health, 200 => ok. Used for status line.
    pub async fn health(&self) -> bool;
}
```

Notes:
- Include `reasoning_content` in responses? llama-server does automatically.
- Accumulate tool_calls fragments by index (types::SseToolCallDelta), joining
  `arguments` fragments byte-exactly.
- `StreamResult` must contain final content, reasoning, assembled
  `Vec<ToolCall>`, finish_reason, usage, timings.
- `max_new_tokens` default 4096.
- `ApiError: Reqwest(reqwest::Error) | NonOkStatus(u16, String) | BadSse(String)`.

Provide a unit test that exercises `stream_chunks` from sse.rs only (no
network). Network calls are handled in integration (daemon e2e), not here.

## src/tools.rs — 7 tools + question

```rust
pub fn definitions() -> Vec<ToolDefinition>;
// includes read, write, edit, bash, grep, find, ls + question
// (question is special-cased by the agent, but include it here)

pub async fn run(
    name: &str,
    arguments_json: &str,
    workdir: std::path::PathBuf,
    timeout_ms: u64,
) -> ToolResult;
```

Tool semantics (mirror pi_agent_rust where useful; keep simple):

- **read**: `{"path": "...", "offset": 0, "limit": 200}` → numbered lines.
  `path` resolves relative to workdir. Must refuse to read outside workdir
  (canonicalize and prefix check).
- **write**: `{"path": "...", "content": "..."}` → create/overwrite file.
- **edit**: `{"path": "...", "old": "...", "new": "..."}` → first occurrence,
  error if old not found.
- **bash**: `{"command": "...", "cwd": null}` → run via tokio::process::Command
  with timeout (default 30000ms), truncate output to ~30k chars with marker.
  Resolve cwd under workdir. `shell=true` semantics — use `sh -c`.
- **grep**: `{"pattern": "...", "path": ".", "glob": null}` → recursive search
  with line numbers, `rg` if available else a simple pure-rust walker grep.
  Keep it simple: pure-rust walker (no external binary dependency) OR `rg`
  with fallback; decide in code with a test.
- **find**: `{"pattern": "*.rs", "path": "."}` → recursive glob listing.
- **ls**: `{"path": "."}` → simple listing with sizes.
- **question**: `run` for "question" returns a ToolResult::err("handled by
  agent") — the agent intercepts this tool before calling `run`.

All tools return `ToolResult`. Errors are ToolResult::err with a readable
message (never panic). Paths always canonicalize within workdir (reject `..`
escapes). Timeouts kill children. Output truncated for bash/grep/find.

Provide unit tests (no network): read/write/edit round-trip in a temp dir,
path escape rejection, ls/find/grep basic, bash `echo`, bash timeout.

Do NOT touch main.rs/types.rs/sse.rs/api.rs/transcript.rs/agent.rs.