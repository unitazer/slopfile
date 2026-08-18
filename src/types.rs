//! Shared types: OpenAI-compatible wire messages, tool definitions, SSE deltas.
//!
//! These types are the contract between modules (`api`, `agent`, `transcript`,
//! `tools`). Keep them lean and OpenAI-shaped (that's what llama-server speaks
//! and what LLMs expect).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Roles ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
  System,
  User,
  Assistant,
  Tool,
}

// ── Tool calls ──────────────────────────────────────────────────────────

/// A tool call issued by the assistant.
///
/// Kept in OpenAI wire shape: `{"id", "type", "function": {"name",
/// "arguments"}}`. The struct fields stay flat internally; custom
/// (De)serialize maps to the nested shape llama-server expects.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
  /// Server-assigned id, e.g. "2p4IMWaD5dEwtO0iFX36MTxBj5cYNF1".
  pub id: String,
  /// Wire-required discriminator; always "function".
  pub type_: String,
  pub name: String,
  /// Raw JSON string of the arguments object (never pre-parsed to Value
  /// because re-serializing must round-trip byte-exactly).
  pub arguments: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FunctionCallWire {
  name: String,
  arguments: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ToolCallWire {
  id: String,
  #[serde(rename = "type", default = "default_tool_call_type")]
  type_: String,
  function: FunctionCallWire,
}

impl serde::Serialize for ToolCall {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    ToolCallWire {
      id: self.id.clone(),
      type_: "function".to_string(),
      function: FunctionCallWire {
        name: self.name.clone(),
        arguments: self.arguments.clone(),
      },
    }
    .serialize(s)
  }
}

impl<'de> serde::Deserialize<'de> for ToolCall {
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    let w = ToolCallWire::deserialize(d)?;
    Ok(Self {
      id: w.id,
      type_: w.type_,
      name: w.function.name,
      arguments: w.function.arguments,
    })
  }
}

impl ToolCall {
  pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
    Self {
      id: id.into(),
      type_: "function".to_string(),
      name: name.into(),
      arguments: arguments.into(),
    }
  }
}

fn default_tool_call_type() -> String {
  "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
  #[serde(rename = "type")]
  pub type_: String,
  pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  pub parameters: Value,
}

// ── Chat messages ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
  pub role: Role,
  /// Text content.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub content: Option<String>,
  /// Assistant messages only: reasoning content (thinking).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reasoning_content: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tool_calls: Option<Vec<ToolCall>>,
  /// Tool messages only: which tool call this answers.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tool_call_id: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
}

impl ChatMessage {
  pub fn user(text: impl Into<String>) -> Self {
    Self {
      role: Role::User,
      content: Some(text.into()),
      reasoning_content: None,
      tool_calls: None,
      tool_call_id: None,
      name: None,
    }
  }

  pub fn assistant(text: impl Into<String>) -> Self {
    Self {
      role: Role::Assistant,
      content: Some(text.into()),
      reasoning_content: None,
      tool_calls: None,
      tool_call_id: None,
      name: None,
    }
  }

  pub fn system(text: impl Into<String>) -> Self {
    Self {
      role: Role::System,
      content: Some(text.into()),
      reasoning_content: None,
      tool_calls: None,
      tool_call_id: None,
      name: None,
    }
  }

  pub fn tool(
    tool_call_id: impl Into<String>,
    name: impl Into<String>,
    text: impl Into<String>,
  ) -> Self {
    Self {
      role: Role::Tool,
      content: Some(text.into()),
      reasoning_content: None,
      tool_calls: None,
      tool_call_id: Some(tool_call_id.into()),
      name: Some(name.into()),
    }
  }
}

// ── SSE delta stream ────────────────────────────────────────────────────

/// One streamed chunk from `/v1/chat/completions`.
#[derive(Debug, Clone, Deserialize)]
pub struct SseChunk {
  #[serde(default)]
  pub choices: Vec<SseChoice>,
  #[serde(default)]
  pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SseChoice {
  #[serde(default)]
  pub delta: SseDelta,
  #[serde(default)]
  pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SseDelta {
  #[serde(default)]
  pub content: Option<String>,
  /// Qwen3 extended-thinking channel (llama-server exposes it).
  #[serde(default)]
  pub reasoning_content: Option<String>,
  #[serde(default)]
  pub tool_calls: Option<Vec<SseToolCallDelta>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SseToolCallDelta {
  #[serde(default)]
  pub index: Option<usize>,
  #[serde(default)]
  pub id: Option<String>,
  #[serde(default)]
  pub r#type: Option<String>,
  #[serde(default)]
  pub function: Option<SseFunctionDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SseFunctionDelta {
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default)]
  pub arguments: Option<String>, // incremental fragments
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Usage {
  #[serde(default)]
  pub prompt_tokens: u32,
  #[serde(default)]
  pub completion_tokens: u32,
}

// ── Aggregated stream result (what the agent loop consumes) ─────────────

#[derive(Debug, Clone, Default)]
pub struct StreamResult {
  pub content: String,
  pub reasoning: String,
  pub tool_calls: Vec<ToolCall>,
  pub finish_reason: Option<String>,
  pub usage: Option<Usage>,
  /// Set when the consumer asked the stream to stop early (abort).
  pub aborted: bool,
}

/// Union of everything a turn encounter can be, used by the agent loop to
/// decide next steps (more text vs. execute tool vs. done).
///
/// `ToolCallFragment`/`Done` payloads are emitted as a preview while the
/// full data is also returned from `chat_stream`; the loop reads the
/// returned copy, so the preview payloads are intentionally untouched.
#[derive(Debug, Clone)]
pub enum StreamEvent {
  Text(String),
  Reasoning(String),
  /// Preview-only: the agent loop reads the returned `StreamResult`
  /// instead, so the payload is intentionally never consumed here.
  #[allow(dead_code)]
  ToolCallFragment(SseToolCallDelta),
  #[allow(dead_code)]
  Done(StreamResult),
}

// ── App config (docker-pi/shared/slopfile.toml) ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
  #[serde(default = "default_confirmations")]
  pub confirmations: String, // "ask" | "yolo"
  #[serde(default = "default_api_base")]
  pub api_base: String,
  #[serde(default = "default_workdir")]
  pub workdir: String,
  #[serde(default = "default_transcripts_dir")]
  pub transcripts_dir: String,
  #[serde(default = "default_sweep_interval_ms")]
  pub sweep_interval_ms: u64,
  #[serde(default = "default_model")]
  pub model: String,
  #[serde(default = "default_tool_timeout_ms")]
  pub tool_timeout_ms: u64,
  #[serde(default = "default_max_context")]
  pub max_context_tokens: u32,
  /// When a user abort interrupts a live stream, restart this llama-server
  /// container via the docker socket. llama.cpp has no cancel endpoint and
  /// does not reliably stop generation on client disconnect, so the
  /// abandoned completion would otherwise keep burning CPU/GPU (bounded
  /// only by its max_tokens). Restarting frees the hardware instantly and
  /// clears the slot for the next turn.
  #[serde(default = "default_abort_restart_server")]
  pub abort_restart_server: bool,
  /// Docker container name of the inference server (compose service
  /// `llama-server` → `clanker-llama`).
  #[serde(default = "default_llama_container")]
  pub llama_container: String,
}

fn default_confirmations() -> String {
  "ask".to_string()
}
fn default_api_base() -> String {
  "http://llama-server:8080".to_string()
}
fn default_workdir() -> String {
  "/home/unitazer/things/clanker".to_string()
}
fn default_transcripts_dir() -> String {
  "/shared/transcripts".to_string()
}
fn default_sweep_interval_ms() -> u64 {
  250
}
fn default_model() -> String {
  "qwen3.8-27b".to_string()
}
fn default_tool_timeout_ms() -> u64 {
  30_000
}
fn default_max_context() -> u32 {
  96_000
}
fn default_abort_restart_server() -> bool {
  true
}
fn default_llama_container() -> String {
  "clanker-llama".to_string()
}

impl Default for AppConfig {
  fn default() -> Self {
    Self {
      confirmations: default_confirmations(),
      api_base: default_api_base(),
      workdir: default_workdir(),
      transcripts_dir: default_transcripts_dir(),
      sweep_interval_ms: default_sweep_interval_ms(),
      model: default_model(),
      tool_timeout_ms: default_tool_timeout_ms(),
      max_context_tokens: default_max_context(),
      abort_restart_server: default_abort_restart_server(),
      llama_container: default_llama_container(),
    }
  }
}

impl AppConfig {
  pub fn load_from_file(path: &str) -> Result<Self, String> {
    match std::fs::read_to_string(path) {
      Ok(text) => toml::from_str(&text).map_err(|e| format!("bad toml: {e}")),
      Err(_) => Ok(Self::default()),
    }
  }
}

// ── Per-session header config (first ```toml fence in the file) ─────────

/// Mirrors AppConfig fields; `None` = fall back to AppConfig. This is what
/// lives in the file header and overrides app settings per session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfig {
  #[serde(default)]
  pub model: Option<String>,
  /// "auto" | "on" | "off". Informational only: llama-server decides
  /// whether to emit reasoning; parsing it keeps the header faithful.
  #[serde(default)]
  pub thinking: Option<String>,
  /// Per-session tool-call timeout; overrides the app-wide
  /// `AppConfig::tool_timeout_ms` when set.
  #[serde(default)]
  pub tool_timeout_ms: Option<u64>,
  #[serde(default)]
  pub tools: Option<Vec<String>>,
  #[serde(default)]
  pub max_context_tokens: Option<u32>,
}

// ── Tool results ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
  pub ok: bool,
  /// Text given back to the model (stdout, file contents, error message…).
  pub output: String,
  /// Extra diagnostics for the status line / user (not shown to the model
  /// if it duplicates `output`).
  #[serde(default)]
  pub meta: Value,
}

impl ToolResult {
  pub fn ok(output: impl Into<String>) -> Self {
    Self {
      ok: true,
      output: output.into(),
      meta: Value::Null,
    }
  }
  pub fn err(output: impl Into<String>) -> Self {
    Self {
      ok: false,
      output: output.into(),
      meta: Value::Null,
    }
  }
}

// ── Live status line (rewritten at daemon turn boundaries at file bottom) ─

/// JSON payload of the `<!-- slopfile:live {...} -->` status line. Only
/// `idle` and `waiting_answer` phases are currently emitted (the phase is
/// set when the daemon releases the file); the streamed view is the file
/// itself.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LiveStatus {
  pub phase: String, // idle | waiting_answer
  pub model: String,
}
