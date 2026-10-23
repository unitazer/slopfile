use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
  System,
  User,
  Assistant,
  Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
  pub id: String,
  pub name: String,
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
      name: w.function.name,
      arguments: w.function.arguments,
    })
  }
}

impl ToolCall {
  pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: impl Into<String>) -> Self {
    Self {
      id: id.into(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
  pub role: Role,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub content: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub reasoning_content: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tool_calls: Option<Vec<ToolCall>>,
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

  #[cfg(test)]
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
  #[serde(default)]
  pub reasoning_content: Option<String>,
  /// OpenAI's and OpenRouter's name for the same stream field. llama.cpp sends
  /// `reasoning_content`, everything else sends `reasoning`.
  #[serde(default, deserialize_with = "reasoning_or_ignored")]
  pub reasoning: Option<String>,
  #[serde(default)]
  pub tool_calls: Option<Vec<SseToolCallDelta>>,
}

/// Some OpenAI-compatible gateways send `reasoning` as a content-block array rather than a string.
/// Reading it as a typed `String` would fail the whole chunk, which kills the stream and throws
/// away an answer that was already arriving -- strictly worse than ignoring a shape we do not
/// understand. Unknown shapes are dropped, as they were before this field existed.
fn reasoning_or_ignored<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
  D: serde::Deserializer<'de>,
{
  Ok(match serde_json::Value::deserialize(d)? {
    serde_json::Value::String(s) => Some(s),
    _ => None,
  })
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
  pub arguments: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Usage {
  #[serde(default)]
  pub prompt_tokens: u32,
  #[serde(default)]
  pub completion_tokens: u32,
  #[serde(default, deserialize_with = "details_or_ignored")]
  pub completion_tokens_details: Option<CompletionTokensDetails>,
}

/// `#[serde(default)]` only covers an *absent* key, so an explicit `null` -- which several
/// gateways send when they have no reasoning to report -- would fail the whole usage chunk. That
/// chunk arrives at the very end of the stream, so the failure mode is an answer that was fully
/// streamed and is then thrown away.
fn details_or_ignored<'de, D>(d: D) -> Result<Option<CompletionTokensDetails>, D::Error>
where
  D: serde::Deserializer<'de>,
{
  Ok(
    serde_json::Value::deserialize(d)
      .ok()
      .and_then(|v| serde_json::from_value(v).ok()),
  )
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct CompletionTokensDetails {
  /// Reasoning tokens are billed as output and, on most providers, come out of the same
  /// `max_tokens` allowance. When they eat the whole thing the visible reply is empty and the
  /// only clue is `finish_reason: "length"` -- the same signal a full context window produces.
  #[serde(default)]
  pub reasoning_tokens: u32,
}

impl Usage {
  /// Did thinking, rather than the context window, stop this turn?
  ///
  /// A reply that hit the output cap with essentially nothing visible in it was truncated by
  /// its own reasoning budget. Pruning the transcript cannot help that, so the two cases have to
  /// be told apart or the harness advises the model to do the one thing that would not help.
  pub fn truncated_by_reasoning(&self, visible_output: &str) -> bool {
    if !visible_output.trim().is_empty() {
      return false;
    }
    match &self.completion_tokens_details {
      Some(d) => u64::from(d.reasoning_tokens) * 2 >= u64::from(self.completion_tokens.max(1)),
      None => false,
    }
  }
}

/// One measured request: the bytes we put on the wire, and the tokens the provider counted for
/// them.
///
/// OpenRouter bills and reports with the model's *native* tokenizer, and returns this on every
/// response, so the ratio between the two is a measured density for this model on this content --
/// not a constant we invented. `chars` is the length of the serialized payload rather than of
/// the message text, which is what makes it comparable: that figure already includes the tool
/// definitions and the per-message framing the provider also counts, both of which a character
/// count of the prose alone is blind to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptSample {
  pub chars: u64,
  pub tokens: u32,
  /// What produced this ratio. Two different models, or two different servers, do not share a
  /// density, so a sample is only usable by the thing that generated it.
  pub endpoint: String,
  pub model: String,
}

impl PromptSample {
  /// Bytes per token, measured. `None` when the sample cannot support a ratio -- no response
  /// yet, or one that came back without usage -- so the caller knows to enforce nothing rather
  /// than fall back to a guess.
  pub fn chars_per_token(&self) -> Option<f64> {
    if self.chars == 0 || self.tokens == 0 {
      return None;
    }
    Some(self.chars as f64 / self.tokens as f64)
  }

  /// The ratio, but only if this sample was taken against the endpoint and model about to be
  /// called. A fence can be retargeted between turns, and carrying one model's density into
  /// another's request is exactly the invented figure this replaced.
  pub fn ratio_for(&self, endpoint: &str, model: &str) -> Option<f64> {
    if self.endpoint == endpoint && self.model == model {
      self.chars_per_token()
    } else {
      None
    }
  }
}

#[derive(Debug, Clone, Default)]
pub struct StreamResult {
  pub content: String,
  pub reasoning: String,
  pub tool_calls: Vec<ToolCall>,
  pub finish_reason: Option<String>,
  pub usage: Option<Usage>,
  pub aborted: bool,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
  Text(String),
  Reasoning(String),
  #[allow(dead_code)]
  ToolCallFragment(SseToolCallDelta),
  #[allow(dead_code)]
  Done(StreamResult),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Confirmations {
  Yolo,
  #[default]
  #[serde(other)]
  Ask,
}

/// How hard the model should think, sent as the OpenAI-compatible `reasoning_effort`.
///
/// llama-server passes this straight into the chat template as a kwarg
/// (`reasoning_effort`), except `none`, which it turns into `enable_thinking=false`.
/// So whether it does anything at all is up to the model's template: a model not
/// trained with it ignores the kwarg silently. Qwen3.8's template accepts
/// `low`/`medium`/`xhigh` (aliasing `high` to `xhigh`) and **raises an exception**
/// on anything else, which surfaces as an HTTP error rather than a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
  /// Omit the field and let the server decide (Qwen3.8 defaults to `xhigh`).
  #[default]
  Auto,
  /// No thinking at all. Sent as `none`, which llama-server turns into
  /// `enable_thinking=false`.
  None,
  Low,
  Medium,
  /// Not a Qwen level, but OpenAI's; Qwen's template aliases it to `xhigh`.
  High,
  Xhigh,
}

impl Thinking {
  /// The value to put in the request, or `None` to omit the field entirely.
  pub fn wire_value(self) -> Option<&'static str> {
    match self {
      Thinking::Auto => None,
      Thinking::None => Some("none"),
      Thinking::Low => Some("low"),
      Thinking::Medium => Some("medium"),
      Thinking::High => Some("high"),
      Thinking::Xhigh => Some("xhigh"),
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Thinking::Auto => "auto",
      Thinking::None => "none",
      Thinking::Low => "low",
      Thinking::Medium => "medium",
      Thinking::High => "high",
      Thinking::Xhigh => "xhigh",
    }
  }
}

impl std::fmt::Display for Thinking {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// A named OpenAI-compatible endpoint, as written in `[providers.<name>]`.
///
/// Only `base_url` is required; everything else has a sensible default for a plain
/// OpenAI-compatible server. `llama = true` opts back in to llama.cpp's private extras
/// (`/models/load`, `/abort`, `/tokenize`), which a hosted provider does not have.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Provider {
  /// Root of the API. A trailing `/v1` is accepted and normalized away, since the request paths
  /// carry it themselves -- so both `https://openrouter.ai/api` and `https://openrouter.ai/api/v1`
  /// mean the same thing.
  pub base_url: String,
  /// Sent on every request to this provider. This is where the API key goes:
  /// `Authorization = "Bearer ${OPENROUTER_API_KEY}"`.
  #[serde(default)]
  pub headers: BTreeMap<String, String>,
  /// This endpoint is a llama.cpp server: probe `/models/load`, `/abort` and `/tokenize`.
  #[serde(default)]
  pub llama: bool,
  /// How to spell the thinking knob. OpenAI reads `reasoning_effort`; OpenRouter's unified
  /// interface is a `reasoning` object, and the two are not interchangeable -- a model can
  /// advertise one and not the other.
  #[serde(default)]
  pub reasoning_style: ReasoningStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningStyle {
  /// `reasoning_effort: "high"` -- OpenAI's shape, and what llama-server wants.
  #[default]
  Effort,
  /// `reasoning: { effort: "high" }` -- OpenRouter's unified shape.
  Object,
}

/// A `Provider` with its environment variables expanded and its base URL normalized: what the
/// HTTP client actually talks to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Endpoint {
  /// Normalized root: no trailing slash, no trailing `/v1`.
  pub base: String,
  pub headers: BTreeMap<String, String>,
  pub llama: bool,
  pub reasoning_style: ReasoningStyle,
}

impl Endpoint {
  /// `llama-server`'s own root, for the implicit provider that `api_base` describes.
  pub fn local(api_base: &str) -> Self {
    Self {
      base: normalize_base(api_base),
      headers: BTreeMap::new(),
      llama: true,
      reasoning_style: ReasoningStyle::Effort,
    }
  }

  /// `POST {base}/v1/...`, tolerating a base the user already spelled with the version in it.
  pub fn url(&self, path: &str) -> String {
    let path = path.trim_start_matches('/');
    if path.starts_with("v1/") {
      format!("{}/{}", self.base, path)
    } else {
      format!("{}/v1/{}", self.base, path)
    }
  }

  /// The raw root, for llama.cpp's endpoints that sit outside `/v1`.
  pub fn raw_url(&self, path: &str) -> String {
    format!("{}/{}", self.base, path.trim_start_matches('/'))
  }
}

/// `http://127.0.0.1:8080/` and `https://openrouter.ai/api/v1` both have to reduce to something we
/// can append `/v1/chat/completions` to without producing `.../v1/v1/...`.
pub fn normalize_base(url: &str) -> String {
  let mut base = url.trim().trim_end_matches('/').to_string();
  if base.to_ascii_lowercase().ends_with("/v1") {
    base.truncate(base.len() - 3);
  }
  base
}

/// Expand `${VAR}` in a header value from the environment, so a key can live outside the config
/// file. An unset variable is an error, not an empty string: dropping the header silently would
/// produce a 401 that reads like a bad key rather than a missing one.
fn expand_env(value: &str, provider: &str, header: &str) -> Result<String, String> {
  if !value.contains("${") {
    return Ok(value.to_string());
  }
  let mut out = String::with_capacity(value.len());
  let mut rest = value;
  while let Some(at) = rest.find("${") {
    out.push_str(&rest[..at]);
    let after = &rest[at + 2..];
    let Some(end) = after.find('}') else {
      return Err(format!(
        "provider `{provider}`: header `{header}` has an unterminated `${{`"
      ));
    };
    let name = &after[..end];
    match std::env::var(name) {
      // Empty is as useless as unset, and the failure it causes is far less legible: compose
      // substitutes an empty string for an unset host variable, which would otherwise produce
      // `Authorization: Bearer ` and a 401 that reads like a wrong key rather than a missing one.
      Ok(v) if !v.is_empty() => out.push_str(&v),
      _ => {
        return Err(format!(
          "provider `{provider}`: header `{header}` wants ${name}, which is {}",
          if std::env::var(name).is_ok() {
            "empty"
          } else {
            "not set"
          },
        ));
      }
    }
    rest = &after[end + 1..];
  }
  out.push_str(rest);
  Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
  #[serde(default)]
  pub confirmations: Confirmations,
  /// The implicit, always-available endpoint. Used whenever a session names no provider, and is
  /// assumed to be llama-server, since that is what it has always pointed at.
  #[serde(default = "default_api_base")]
  pub api_base: String,
  /// Named endpoints a session can select by putting `provider = "<name>"` in its fence.
  #[serde(default)]
  pub providers: BTreeMap<String, Provider>,
  /// Which named endpoint a session that names none gets. `None` keeps `api_base`.
  #[serde(default)]
  pub provider: Option<String>,
  #[serde(default = "default_workdir")]
  pub workdir: String,
  #[serde(default = "default_transcripts_dir")]
  pub transcripts_dir: String,
  #[serde(default = "default_skills_dir")]
  pub skills_dir: String,
  #[serde(default = "default_sweep_interval_ms")]
  pub sweep_interval_ms: u64,
  /// Stamped into the fence of every new session. Renamed from a bare `model`, which read
  /// like a global switch but never was: a session pins its own model at creation, so this
  /// only ever affects sessions created *after* you change it.
  #[serde(default = "default_session_model", alias = "model")]
  pub default_session_model: String,
  #[serde(default = "default_tool_timeout_ms")]
  pub tool_timeout_ms: u64,
  #[serde(default = "default_max_context")]
  pub max_context_tokens: u32,
  #[serde(default = "default_abort_restart_server")]
  pub abort_restart_server: bool,
  #[serde(default = "default_llama_container")]
  pub llama_container: String,
  /// Turns one `sf send` may spend before the harness stops the burst. 0, the default, means no
  /// limit: the turn cap that used to sit here at a fixed 20 was a band-aid for a `bash` bug that
  /// forced the model to poll backgrounded jobs, and it outlived the bug.
  #[serde(default)]
  pub burst_max_turns: u32,
  /// Tell the model how large its context window currently is.
  ///
  /// On by default. A model that is asked to prune its own transcript has no way to know how full
  /// it is, and the only other signal arrives mid-response once the answer is already truncated --
  /// so compaction timing becomes guesswork. Each tool result carries `ctx:N`, the count as of the
  /// request that made the call, and the system prompt explains how to read it.
  #[serde(default = "default_true")]
  pub tell_model_context_size: bool,
  #[serde(default = "default_subagent_max_depth")]
  pub subagent_max_depth: u32,
}

fn default_api_base() -> String {
  "http://127.0.0.1:8080".to_string()
}
fn default_workdir() -> String {
  "workspace".to_string()
}
fn default_transcripts_dir() -> String {
  "shared/transcripts".to_string()
}
fn default_skills_dir() -> String {
  "shared/skills".to_string()
}
fn default_sweep_interval_ms() -> u64 {
  250
}
fn default_session_model() -> String {
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
fn default_subagent_max_depth() -> u32 {
  2
}
fn default_true() -> bool {
  true
}

impl Default for AppConfig {
  fn default() -> Self {
    Self {
      confirmations: Confirmations::default(),
      api_base: default_api_base(),
      providers: BTreeMap::new(),
      provider: None,
      workdir: default_workdir(),
      transcripts_dir: default_transcripts_dir(),
      skills_dir: default_skills_dir(),
      sweep_interval_ms: default_sweep_interval_ms(),
      default_session_model: default_session_model(),
      tool_timeout_ms: default_tool_timeout_ms(),
      max_context_tokens: default_max_context(),
      abort_restart_server: default_abort_restart_server(),
      llama_container: default_llama_container(),
      burst_max_turns: 0,
      tell_model_context_size: true,
      subagent_max_depth: default_subagent_max_depth(),
    }
  }
}

impl AppConfig {
  pub fn load_from_file(path: &str) -> Result<Self, String> {
    match std::fs::read_to_string(path) {
      Ok(text) => Self::from_toml(&text).map_err(|e| format!("bad toml: {e}")),
      Err(_) => Ok(Self::default()),
    }
  }

  /// `model` is an accepted alias for `default_session_model`, but serde treats both as the same
  /// field -- so a file carrying the old name and the new one fails as a duplicate, and the caller
  /// falls back to `AppConfig::default()`. That throws away `workdir`, `transcripts_dir`, the
  /// provider table and the context window, with only a warning to show for it. During the rename
  /// that is exactly the file someone is most likely to have half-migrated, so the old key is
  /// dropped when the new one is present and the rest of the file is kept either way.
  fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
    let mut value: toml::Value = toml::from_str(text)?;
    if let Some(table) = value.as_table_mut()
      && table.contains_key("default_session_model")
    {
      table.remove("model");
    }
    value.try_into()
  }

  /// The endpoint a session should talk to. `name` is the session's `provider` field; `None`
  /// falls back to the app-wide `provider`, and then to `api_base`. `extra` is the session's own
  /// `headers` table, applied last so a transcript can override or add to the provider's.
  ///
  /// `Err` rather than a silent fallback: a session pointed at a provider that does not exist is
  /// a typo, and answering it with some other endpoint would spend real money on the wrong model.
  pub fn endpoint(
    &self,
    name: Option<&str>,
    extra: &BTreeMap<String, String>,
  ) -> Result<Endpoint, String> {
    let name = match name.or(self.provider.as_deref()) {
      Some(n) => n,
      None => {
        let mut endpoint = Endpoint::local(&self.api_base);
        for (header, value) in extra {
          // Lower-cased for the same reason as the named-provider path below.
          endpoint.headers.insert(
            header.to_ascii_lowercase(),
            expand_env(value, "this session", header)?,
          );
        }
        return Ok(endpoint);
      }
    };
    let provider = self.providers.get(name).ok_or_else(|| {
      format!(
        "unknown provider `{name}`; configured: {}",
        self.provider_names()
      )
    })?;
    if provider.base_url.trim().is_empty() {
      return Err(format!("provider `{name}` has no base_url"));
    }
    let mut headers = BTreeMap::new();
    for (header, value) in provider.headers.iter().chain(extra) {
      // Folded to lower case because header names are case-insensitive but a BTreeMap is not, and
      // the two get merged into a `HeaderMap` later -- which *appends* rather than replaces. Left
      // as written, a provider's `authorization` and a session's `Authorization` would both go on
      // the wire, and the documented "the session wins" rule would quietly not hold.
      headers.insert(
        header.to_ascii_lowercase(),
        expand_env(value, name, header)?,
      );
    }
    Ok(Endpoint {
      base: normalize_base(&provider.base_url),
      headers,
      llama: provider.llama,
      reasoning_style: provider.reasoning_style,
    })
  }

  /// `api_base`, or whatever `provider` points at -- for a log line that should name the endpoint
  /// actually in use without failing when it is misconfigured.
  pub fn endpoint_or_api_base(&self) -> Endpoint {
    self.endpoint(None, &BTreeMap::new()).unwrap_or_else(|e| {
      tracing::warn!("{e}; falling back to api_base");
      Endpoint::local(&self.api_base)
    })
  }

  fn provider_names(&self) -> String {
    if self.providers.is_empty() {
      "none (set [providers.<name>] in the config)".to_string()
    } else {
      let mut names: Vec<&str> = self.providers.keys().map(String::as_str).collect();
      names.sort_unstable();
      names.join(", ")
    }
  }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
  #[serde(default)]
  pub provider: Option<String>,
  /// Headers applied on top of the provider's own, for this session only. The escape hatch for
  /// "I don't want to put my key in the config file": the key lands in the transcript, which is
  /// gitignored and never sent to the model (the fence is not part of any request).
  #[serde(default)]
  pub headers: BTreeMap<String, String>,
  #[serde(default)]
  pub model: Option<String>,
  #[serde(default)]
  pub thinking: Thinking,
  #[serde(default)]
  pub tool_timeout_ms: Option<u64>,
  #[serde(default)]
  pub tools: Option<Vec<String>>,
  #[serde(default)]
  pub tools_excluded: Vec<String>,
  #[serde(default)]
  pub max_context_tokens: Option<u32>,
  /// Per-session override for `AppConfig::burst_max_turns`. 0 means uncapped.
  #[serde(default)]
  pub burst_max_turns: Option<u32>,
  /// Per-session override for `AppConfig::tell_model_context_size`.
  #[serde(default)]
  pub tell_model_context_size: Option<bool>,
}

impl AppConfig {
  /// Whether this session is told its context size. The fence wins, then the app-wide setting.
  pub fn tell_context_size(&self, session: &SessionConfig) -> bool {
    session
      .tell_model_context_size
      .unwrap_or(self.tell_model_context_size)
  }
}

impl SessionConfig {
  pub fn allows_tool(&self, name: &str) -> bool {
    !self.tools_excluded.iter().any(|t| t == name)
      && self
        .tools
        .as_ref()
        .is_none_or(|l| l.iter().any(|t| t == name))
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
  pub ok: bool,
  pub output: String,
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

#[cfg(test)]
mod tests {
  use super::*;

  fn cfg_with(provider: &str) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.providers.insert(
      provider.to_string(),
      Provider {
        base_url: "https://openrouter.ai/api/v1".to_string(),
        headers: BTreeMap::new(),
        llama: false,
        reasoning_style: ReasoningStyle::Effort,
      },
    );
    cfg
  }

  #[test]
  fn base_is_normalized_so_the_version_is_never_doubled() {
    assert_eq!(
      normalize_base("http://127.0.0.1:8080"),
      "http://127.0.0.1:8080"
    );
    assert_eq!(
      normalize_base("https://openrouter.ai/api/v1"),
      "https://openrouter.ai/api"
    );
    assert_eq!(
      normalize_base("https://api.openai.com/v1/"),
      "https://api.openai.com"
    );
    let ep = Endpoint::local("https://openrouter.ai/api/v1");
    assert_eq!(
      ep.url("chat/completions"),
      "https://openrouter.ai/api/v1/chat/completions"
    );
    assert_eq!(ep.raw_url("abort"), "https://openrouter.ai/api/abort");
  }

  #[test]
  fn no_provider_named_means_the_llama_server() {
    let ep = AppConfig::default()
      .endpoint(None, &BTreeMap::new())
      .unwrap();
    assert_eq!(ep.base, "http://127.0.0.1:8080");
    assert!(ep.llama, "api_base is a llama-server by assumption");
    assert!(ep.headers.is_empty());
  }

  #[test]
  fn a_session_names_its_own_provider() {
    let cfg = cfg_with("openrouter");
    let ep = cfg.endpoint(Some("openrouter"), &BTreeMap::new()).unwrap();
    assert_eq!(ep.base, "https://openrouter.ai/api");
    assert!(!ep.llama, "a hosted provider has no /tokenize or /abort");
  }

  #[test]
  fn the_app_default_covers_sessions_that_name_none() {
    let mut cfg = cfg_with("openrouter");
    cfg.provider = Some("openrouter".to_string());
    assert_eq!(
      cfg.endpoint(None, &BTreeMap::new()).unwrap().base,
      "https://openrouter.ai/api"
    );
    // ...but a session that names its own provider still wins.
    assert!(cfg.endpoint(Some("nope"), &BTreeMap::new()).is_err());
  }

  #[test]
  fn a_typo_is_an_error_rather_than_a_fallback_to_the_wrong_endpoint() {
    let err = cfg_with("openrouter")
      .endpoint(Some("openroter"), &BTreeMap::new())
      .unwrap_err();
    assert!(err.contains("unknown provider `openroter`"), "{err}");
    assert!(
      err.contains("openrouter"),
      "the error lists what does exist: {err}"
    );

    let err = AppConfig::default()
      .endpoint(Some("whatever"), &BTreeMap::new())
      .unwrap_err();
    assert!(err.contains("[providers.<name>]"), "{err}");
  }

  #[test]
  fn headers_carry_the_key_from_the_environment() {
    // Safety: single-threaded at this point, and the variable is unique to this test.
    unsafe { std::env::set_var("SLOPFILE_TEST_KEY", "sk-test-123") };
    let mut cfg = cfg_with("openrouter");
    cfg.providers.get_mut("openrouter").unwrap().headers = BTreeMap::from([(
      "Authorization".to_string(),
      "Bearer ${SLOPFILE_TEST_KEY}".to_string(),
    )]);
    let ep = cfg.endpoint(Some("openrouter"), &BTreeMap::new()).unwrap();
    assert_eq!(ep.headers["authorization"], "Bearer sk-test-123");
    unsafe { std::env::remove_var("SLOPFILE_TEST_KEY") };
  }

  #[test]
  fn an_unset_variable_is_an_error_not_a_silently_dropped_auth_header() {
    let mut cfg = cfg_with("openrouter");
    cfg.providers.get_mut("openrouter").unwrap().headers = BTreeMap::from([(
      "Authorization".to_string(),
      "Bearer ${SLOPFILE_TEST_KEY_UNSET}".to_string(),
    )]);
    let err = cfg
      .endpoint(Some("openrouter"), &BTreeMap::new())
      .unwrap_err();
    assert!(err.contains("SLOPFILE_TEST_KEY_UNSET"), "{err}");
  }

  #[test]
  fn a_provider_needs_a_base_url() {
    let mut cfg = AppConfig::default();
    cfg.providers.insert(
      "empty".to_string(),
      Provider {
        base_url: "  ".to_string(),
        ..Default::default()
      },
    );
    assert!(
      cfg
        .endpoint(Some("empty"), &BTreeMap::new())
        .unwrap_err()
        .contains("no base_url")
    );
  }

  #[test]
  fn a_transcript_can_carry_its_own_key_and_win_over_the_providers() {
    let mut cfg = cfg_with("openrouter");
    cfg.providers.get_mut("openrouter").unwrap().headers = BTreeMap::from([
      ("Authorization".to_string(), "Bearer from-env".to_string()),
      ("X-Title".to_string(), "from-config".to_string()),
    ]);
    let mut from_session =
      BTreeMap::from([("Authorization".to_string(), "sk-or-inline".to_string())]);
    let ep = cfg.endpoint(Some("openrouter"), &from_session).unwrap();
    assert_eq!(
      ep.headers["authorization"], "sk-or-inline",
      "the transcript is the thing you actually control per session"
    );
    assert_eq!(
      ep.headers["x-title"], "from-config",
      "headers the transcript says nothing about are inherited"
    );
    // And with nothing in the transcript, the provider's own value stands.
    from_session.clear();
    assert_eq!(
      cfg
        .endpoint(Some("openrouter"), &from_session)
        .unwrap()
        .headers["authorization"],
      "Bearer from-env"
    );
  }

  #[test]
  fn a_transcript_header_also_reaches_the_implicit_llama_endpoint() {
    // Naming no provider means api_base, but a key in the fence should still be sent.
    let extra = BTreeMap::from([("Authorization".to_string(), "sk-local".to_string())]);
    let ep = AppConfig::default().endpoint(None, &extra).unwrap();
    assert_eq!(ep.base, "http://127.0.0.1:8080");
    assert_eq!(ep.headers["authorization"], "sk-local");
    assert!(ep.llama);
  }

  #[test]
  fn an_empty_variable_is_treated_as_missing_not_as_a_blank_key() {
    // Compose substitutes an empty string for an unset host variable, which would otherwise
    // produce `Authorization: Bearer ` and a 401 that reads like a wrong key.
    unsafe { std::env::set_var("SLOPFILE_TEST_EMPTY", "") };
    let mut cfg = cfg_with("openrouter");
    cfg.providers.get_mut("openrouter").unwrap().headers = BTreeMap::from([(
      "Authorization".to_string(),
      "Bearer ${SLOPFILE_TEST_EMPTY}".to_string(),
    )]);
    let err = cfg
      .endpoint(Some("openrouter"), &BTreeMap::new())
      .unwrap_err();
    assert!(err.contains("which is empty"), "{err}");
    unsafe { std::env::remove_var("SLOPFILE_TEST_EMPTY") };
  }

  #[test]
  fn a_provider_block_parses_out_of_the_config_file() {
    let cfg: AppConfig = toml::from_str(
      r#"
      api_base = "http://127.0.0.1:8080"
      provider = "openrouter"

      [providers.openrouter]
      base_url = "https://openrouter.ai/api/v1"

      [providers.openrouter.headers]
      Authorization = "Bearer sk-or-v1-literal"
      "HTTP-Referer" = "http://localhost"

      [providers.a_second_llama]
      base_url = "http://gpu-box:8080"
      llama = true
      "#,
    )
    .unwrap();
    assert_eq!(cfg.providers.len(), 2);
    assert!(cfg.providers["a_second_llama"].llama);
    let ep = cfg.endpoint(None, &BTreeMap::new()).unwrap();
    assert_eq!(ep.base, "https://openrouter.ai/api");
    // Lower-cased on the way in: header names are case-insensitive, and folding them here is what
    // makes a session's `Authorization` override a provider's `authorization` instead of joining
    // it as a second copy on the wire.
    assert_eq!(ep.headers["authorization"], "Bearer sk-or-v1-literal");
    assert_eq!(ep.headers["http-referer"], "http://localhost");
  }
}

#[cfg(test)]
mod alias_tests {
  use super::*;

  /// Renaming `model` -> `default_session_model` would otherwise silently reset every deployed
  /// config to the built-in default, because `AppConfig` ignores unknown keys. The alias keeps an
  /// old config file working unchanged.
  #[test]
  fn the_old_model_key_still_stamps_new_sessions() {
    let cfg: AppConfig = toml::from_str(r#"model = "qwen3.8-27b""#).unwrap();
    assert_eq!(cfg.default_session_model, "qwen3.8-27b");
  }

  #[test]
  fn the_new_key_wins_and_an_unset_one_falls_back_to_the_built_in_default() {
    let cfg: AppConfig =
      toml::from_str(r#"default_session_model = "qwen/qwen3-235b-a22b""#).unwrap();
    assert_eq!(cfg.default_session_model, "qwen/qwen3-235b-a22b");
    assert_eq!(AppConfig::default().default_session_model, "qwen3.8-27b");
  }
}

#[cfg(test)]
mod sample_tests {
  use super::*;

  #[test]
  fn a_sample_that_cannot_support_a_ratio_yields_none_not_a_default() {
    // The whole design rests on never substituting a constant. A degenerate sample has to decline
    // rather than quietly become 4.0.
    assert_eq!(PromptSample::default().chars_per_token(), None);
    assert_eq!(
      PromptSample {
        chars: 100,
        tokens: 0,
        ..Default::default()
      }
      .chars_per_token(),
      None
    );
    assert_eq!(
      PromptSample {
        chars: 0,
        tokens: 100,
        ..Default::default()
      }
      .chars_per_token(),
      None
    );
    assert_eq!(
      PromptSample {
        chars: 400,
        tokens: 100,
        ..Default::default()
      }
      .chars_per_token(),
      Some(4.0)
    );
  }

  /// A fence can be retargeted between turns, and the daemon re-resolves it every turn. Carrying
  /// one model's density into another's request is precisely the invented figure this design
  /// exists to avoid -- and it is silently wrong, because nothing about it looks like a guess.
  #[test]
  fn a_sample_is_only_usable_by_the_endpoint_and_model_that_produced_it() {
    let sample = PromptSample {
      chars: 4000,
      tokens: 1000,
      endpoint: "https://openrouter.ai/api".to_string(),
      model: "qwen3.8-27b".to_string(),
    };
    assert_eq!(
      sample.ratio_for("https://openrouter.ai/api", "qwen3.8-27b"),
      Some(4.0)
    );
    assert_eq!(
      sample.ratio_for("https://openrouter.ai/api", "qwen/qwen3-235b-a22b"),
      None,
      "a different model does not share a tokenizer"
    );
    assert_eq!(
      sample.ratio_for("http://127.0.0.1:8080", "qwen3.8-27b"),
      None,
      "a different endpoint does not share one either"
    );
  }

  #[test]
  fn an_empty_reply_is_only_reasoning_truncation_when_thinking_ate_the_budget() {
    let reasoning = Usage {
      prompt_tokens: 500,
      completion_tokens: 4000,
      completion_tokens_details: Some(CompletionTokensDetails {
        reasoning_tokens: 3990,
      }),
    };
    assert!(
      reasoning.truncated_by_reasoning(""),
      "4000 tokens out, 3990 of them thinking, nothing visible: that is the budget, not the context"
    );

    let mixed = Usage {
      completion_tokens: 4000,
      completion_tokens_details: Some(CompletionTokensDetails {
        reasoning_tokens: 1000,
      }),
      ..Default::default()
    };
    assert!(!mixed.truncated_by_reasoning(""));

    let wrote_something = Usage {
      completion_tokens: 4000,
      completion_tokens_details: Some(CompletionTokensDetails {
        reasoning_tokens: 3990,
      }),
      ..Default::default()
    };
    assert!(
      !wrote_something.truncated_by_reasoning("here is the answer"),
      "a reply that exists was not stopped by its thinking"
    );

    let no_detail = Usage {
      completion_tokens: 4000,
      ..Default::default()
    };
    assert!(
      !no_detail.truncated_by_reasoning(""),
      "without usage details we cannot claim reasoning caused it"
    );
  }
}
