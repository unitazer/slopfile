//! slopfile daemon: watches the transcripts dir, drives each session `.md`
//! through generation turns against llama-server, streaming deltas into the
//! file. The file is the entire UI — the daemon only ever writes below the
//! last `@done` marker (or replaces daemon-owned partial output).

mod api;
mod lex;
mod questions;
mod sse;
mod tools;
mod transcript;
mod types;

use api::LlamaClient;
use clap::{Parser, Subcommand};
use lex::{lex, lex_line, DONE_MARKER, TokKind};
use questions::parse_answers;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};
use transcript::{strip_live_line, ParsedFile};
use types::{AppConfig, ChatMessage, Role, ToolCall};

#[derive(Parser)]
#[command(
  name = "slopfile",
  about = "file-as-UI coding agent daemon: streams llama-server responses into session .md files"
)]
struct Cli {
  #[command(subcommand)]
  command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
  /// Write the default session template (header + system prompt) into a new
  /// or empty .md file. Refuses to touch a non-empty file.
  Init {
    /// Path of the session file to initialize
    path: String,
  },
  /// Interrupt the agent on a session file: append the abort marker; the
  /// daemon stops the current stream, closes the turn with @done and goes
  /// idle.
  Abort {
    /// Path of the session file to abort
    path: String,
  },
}

/// Host path; the container variant sets `SLOPFILE_CONFIG` to
/// `/shared/slopfile.toml`.
const APP_CONFIG_PATH: &str = "/home/unitazer/things/clanker/docker-pi/shared/slopfile.toml";

/// Load app config, then apply env overrides so the same file works on the
/// host and inside the compose network:
///   SLOPFILE_CONFIG      path to the toml (default APP_CONFIG_PATH)
///   SLOPFILE_API_BASE    e.g. http://llama-server:8080 (container)
///   SLOPFILE_TRANSCRIPTS transcripts dir
///   SLOPFILE_WORKDIR     tool working directory
fn load_app_config() -> AppConfig {
  let path = std::env::var("SLOPFILE_CONFIG").unwrap_or_else(|_| APP_CONFIG_PATH.to_string());
  let mut cfg = AppConfig::load_from_file(&path).unwrap_or_else(|e| {
    warn!("could not load app config ({e}); using defaults");
    AppConfig::default()
  });
  if let Ok(v) = std::env::var("SLOPFILE_API_BASE") {
    cfg.api_base = v;
  }
  if let Ok(v) = std::env::var("SLOPFILE_TRANSCRIPTS") {
    cfg.transcripts_dir = v;
  }
  if let Ok(v) = std::env::var("SLOPFILE_WORKDIR") {
    cfg.workdir = v;
  }
  if let Ok(v) = std::env::var("SLOPFILE_ABORT_RESTART") {
    cfg.abort_restart_server = v != "0" && !v.eq_ignore_ascii_case("false");
  }
  if let Ok(v) = std::env::var("SLOPFILE_LLAMA_CONTAINER") {
    cfg.llama_container = v;
  }
  cfg
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "slopfile=info,warn".into()),
    )
    .init();

  let app_cfg = load_app_config();
  let cli = Cli::parse();
  if let Some(Command::Init { path }) = cli.command {
    return cmd_init(&path, &app_cfg);
  }
  if let Some(Command::Abort { path }) = cli.command {
    return cmd_abort(&path);
  }

  info!(
    "slopfile: workdir={} transcripts={} model={} api={} sweep={}ms confirmations={}",
    app_cfg.workdir,
    app_cfg.transcripts_dir,
    app_cfg.model,
    app_cfg.api_base,
    app_cfg.sweep_interval_ms,
    app_cfg.confirmations
  );
  std::fs::create_dir_all(&app_cfg.transcripts_dir).ok();

  let busy: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
  // Error throttle: skip a file for 5 s after an error so a broken file
  // doesn't hammer the API every sweep.
  let throttled: Arc<Mutex<HashMap<PathBuf, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

  loop {
    let files = scan_transcripts(&app_cfg.transcripts_dir).unwrap_or_default();
    for path in files {
      let mut guard = busy.lock().await;
      if guard.contains(&path) {
        continue;
      }
      if let Some(at) = throttled.lock().await.get(&path)
        && at.elapsed() < Duration::from_secs(5)
      {
        continue;
      }
      guard.insert(path.clone());
      drop(guard);

      let cfg = app_cfg.clone();
      let busy = busy.clone();
      let throttled = throttled.clone();
      tokio::spawn(async move {
        let client = LlamaClient::new(&cfg.api_base, &cfg.model);
        if let Err(e) = drive_file(&path, &cfg, &client).await {
          error!("session {}: {e}", path.display());
          throttled.lock().await.insert(path.clone(), Instant::now());
        }
        busy.lock().await.remove(&path);
      });
    }
    sleep(Duration::from_millis(app_cfg.sweep_interval_ms)).await;
  }
}

fn scan_transcripts(dir: &str) -> Result<Vec<PathBuf>, String> {
  let mut out = Vec::new();
  for entry in std::fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))? {
    let entry = entry.map_err(|e| e.to_string())?;
    let p = entry.path();
    if p.is_file() && p.extension().is_some_and(|e| e == "md") {
      out.push(p);
    }
  }
  out.sort();
  Ok(out)
}

// ── Per-file driving ────────────────────────────────────────────────────

/// Drive one session file until it is quiescent (either the turn completed
/// with `@done`, or the daemon is waiting on the user).
async fn drive_file(path: &Path, cfg: &AppConfig, client: &LlamaClient) -> Result<(), String> {
  let mut turns = 0;
  loop {
    turns += 1;
    if turns > 20 {
      warn!("{path:?}: giving up after {turns} turns");
      return Ok(());
    }

    let text = read_file(path)?;

    // A fresh empty file is a brand-new session: lay down the default
    // template (header + system prompt). Re-check the disk so we never
    // clobber text the user typed between the two reads.
    if text.trim().is_empty() {
      if read_file(path)?.trim().is_empty() {
        write_file(path, session_template(cfg))?;
        info!("→ {path:?}: initialized empty session");
      }
      return Ok(());
    }

    // Abort marker present while idle: nothing is running, just drop the
    // marker line and refresh the status. (During a stream the marker is
    // handled inside StreamWriter; this path covers the in-between state.)
    if transcript::abort_marker_present(&text) {
      strip_abort_marker(path, &text)?;
      return Ok(());
    }

    let pf = transcript::parse_file(&text);
    if !has_work(&pf, &text, cfg) {
      return Ok(());
    }

    // 1) Resolve pending QUESTION blocks (answered by the user, or yolo).
    if !pf.pending_questions.is_empty() {
      if questions_resolvable(&pf, cfg) {
        resolve_questions(path, &text, &pf, cfg)?;
        continue;
      }
      // Waiting for the user to answer.
      write_status(path, &pf, "waiting_answer")?;
      return Ok(());
    }

    // 2) Promote drafted tail text into a `### You` history section.
    if let Some(draft) = draft_tail_after_done(&text)
      && !draft.trim().is_empty()
    {
      promote_draft(path, &text)?;
      continue;
    }

    // 3) Generation turn: cut dangling partial output, stream a response.
    let outcome = run_generation_turn(path, &text, &pf, cfg, client).await?;
    match outcome {
      TurnOutcome::Done => return Ok(()),
      TurnOutcome::AwaitsNextTurn => continue,
    }
  }
}

fn has_work(pf: &ParsedFile, text: &str, cfg: &AppConfig) -> bool {
  if pf.dangling.is_some() {
    return true;
  }
  if !pf.pending_questions.is_empty() {
    // Answered questions = work (resolve into history); unanswered ones =
    // waiting on the user (no daemon work).
    return questions_resolvable(pf, cfg);
  }
  if let Some(draft) = draft_tail_after_done(text)
    && !draft.trim().is_empty()
  {
    return true;
  }
  // A `### You` / other section below the last @done (user wrote their own
  // header, or promotion happened) still needs work.
  if let Some(section) = section_below_done(text)
    && !section.trim().is_empty()
  {
    return true;
  }
  // Fresh file with a first user turn and no `@done` yet.
  if transcript::find_done_marker(text).is_none() {
    return pf
      .history
      .last()
      .is_some_and(|m| m.role == Role::User || m.role == Role::Tool);
  }
  false
}

/// Raw user text below the last `@done` marker (status line and daemon-owned
/// comment lines stripped).
fn section_below_done(text: &str) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  let toks = lex(&body);
  let done = toks.iter().rfind(|t| t.kind == TokKind::Done)?;
  let tail = &body[done.offset + lex::DONE_MARKER.len()..];
  let user_tail = lex(tail)
    .into_iter()
    .filter(|t| !t.kind.is_daemon_owned())
    .map(|t| t.text)
    .collect::<Vec<_>>()
    .join("\n")
    .trim()
    .to_string();
  if user_tail.is_empty() {
    return None;
  }
  if user_tail.starts_with(questions::BLOCK_HEADER) {
    return None;
  }
  Some(user_tail)
}

enum TurnOutcome {
  Done,
  AwaitsNextTurn,
}

/// Stream one assistant response into the file. May end with:
/// - plain text → `@done` closing marker (Done)
/// - tool calls → executed, blocks written (AwaitsNextTurn so the loop
///   continues with the tool results in history)
/// - `question` → QUESTIONS block rendered in tail (AwaitsNextTurn; next
///   loop pass sees pending questions and waits).
async fn run_generation_turn(
  path: &Path,
  text: &str,
  pf: &ParsedFile,
  cfg: &AppConfig,
  client: &LlamaClient,
) -> Result<TurnOutcome, String> {
  // Cut daemon-owned partial output (dangling) so we stream a fresh
  // response: everything up to the end of the dangling `### Assistant`
  // heading stays (the heading is the resume point), the partial text below
  // it is dropped and re-asked. StreamWriter rewrites the WHOLE file on
  // every flush, so user-owned history and drafts above the heading are
  // preserved verbatim.
  let (body, live) = strip_live_line(text);
  let base_text = match &pf.dangling {
    Some(d) => body[..d.resume_offset.min(body.len())].to_string(),
    None => body.clone(),
  };

  // Messages: history + resume hint for dangling output + any in-flight
  // below-@done sections (drafts promoted to `### You`). The below-@done
  // region is appended EXACTLY ONCE here — `parse_file` history already
  // stops at the last `@done`.
  let mut messages = pf.history.clone();
  if transcript::find_done_marker(text).is_some()
    && let Some(section) = section_below_done(text)
  {
    for m in transcript::tail_sections(&section) {
      messages.push(m);
    }
  }
  if let Some(d) = &pf.dangling
    && !d.partial_text.trim().is_empty()
  {
    messages.push(ChatMessage::user(format!(
      "[resume] previous assistant output was interrupted; continue exactly where it stopped:\n```\n{}\n```",
      d.partial_text.trim_end()
    )));
  }

  // Long sessions: drop the OLDEST turns from the request (never the file)
  // so the prompt fits the context window. Keeps the system message and
  // always cuts at a clean turn boundary.
  let context_budget = pf
    .config
    .max_context_tokens
    .unwrap_or(cfg.max_context_tokens);
  messages = truncate_messages(messages, context_budget);

  let max_tokens = context_budget / 4;
  let tools = pf.config.tools.as_ref().map(|t| !t.is_empty()).unwrap_or(true);
  let tool_defs = tools.then(tools::definitions);

  // First write: `### Assistant` header (+ thinking open if the first delta
  // is reasoning — we handle markers in the event closure).
  let mut active = StreamWriter::new(path, &base_text, live.map(|(_, t)| t));

  info!("→ {path:?}: generating (messages={})", messages.len());
  let result = client
    .chat_stream(
      &messages,
      tool_defs.as_deref(),
      max_tokens,
      |ev| active.handle(&ev),
      || file_has_abort(path),
    )
    .await
    .map_err(|e| format!("chat_stream: {e}"))?;

  if result.aborted {
    // User aborted mid-stream: close the turn and stop; no tools, no @done
    // ride-along continuation.
    active.finish_section();
    let text_now = read_file(path)?;
    let (body, live) = strip_live_line(&text_now);
    let mut out = without_abort_lines(&body);
    if !out.ends_with('\n') {
      out.push('\n');
    }
    if !out.trim_end().ends_with(DONE_MARKER) {
      out.push_str(&format!("{DONE_MARKER}\n"));
    }
    if let Some((_, live)) = live {
      out.push('\n');
      out.push_str(&live);
    }
    write_file(path, out)?;
    write_status(path, pf, "idle")?;
    // The abandoned completion keeps decoding on the server (llama.cpp has
    // no cancel endpoint; disconnect is only noticed late, if ever) — pin
    // a core and occupying the slot for minutes. Restart the inference
    // container so the hardware is freed now.
    if cfg.abort_restart_server {
      // Blocking socket I/O; keep it off the async runtime so concurrent
      // sessions' streams are not stalled by the restart roundtrip.
      let container = cfg.llama_container.clone();
      let res = tokio::task::spawn_blocking(move || {
        restart_llama_server("/var/run/docker.sock", &container)
      })
      .await;
      match res {
        Ok(Ok(())) => info!("✗ {path:?}: aborted (llama-server restarted)"),
        Ok(Err(e)) => warn!("{path:?}: aborted but couldn't restart llama-server: {e}"),
        Err(e) => warn!("{path:?}: aborted, llama restart task failed: {e}"),
      }
    } else {
      info!("✗ {path:?}: aborted");
    }
    return Ok(TurnOutcome::Done);
  }

  active.finish_section();

  if result.finish_reason.as_deref() == Some("length") {
    // The model hit the token cap mid-turn. Close the turn and stop: the
    // partial output stays, a note tells the user to trim old turns, and the
    // daemon goes idle. Any partially-streamed tool calls are NOT executed
    // (their argument JSON may be truncated).
    let text_now = read_file(path)?;
    let (body, live) = strip_live_line(&text_now);
    let mut out = body;
    if !out.ends_with('\n') {
      out.push('\n');
    }
    out.push_str(&format!("{DONE_MARKER}\n"));
    out.push_str(&format!("{CONTEXT_FULL_NOTE}\n"));
    if let Some((_, live)) = live {
      out.push('\n');
      out.push_str(&live);
    }
    write_file(path, out)?;
    write_status(path, pf, "idle")?;
    warn!("⚠ {path:?}: context exhausted mid-turn (finish_reason=length)");
    return Ok(TurnOutcome::Done);
  }

  if !result.tool_calls.is_empty() {
    let is_question = result.tool_calls.iter().any(|t| t.name == "question");
    if is_question {
      render_questions_block(path, &result.tool_calls)?;
      return Ok(TurnOutcome::AwaitsNextTurn);
    }
    write_tool_blocks(path, &result.tool_calls, cfg, pf).await?;
    info!("→ {path:?}: {} tool call(s) executed", result.tool_calls.len());
    return Ok(TurnOutcome::AwaitsNextTurn);
  }

  // Plain-text completion: close the turn with `@done`.
  let final_text = read_file(path)?;
  let (body, live) = strip_live_line(&final_text);
  let mut out = body;
  if !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(&format!("{DONE_MARKER}\n"));
  if let Some((_, live)) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)?;
  write_status(path, pf, "idle")?;
  info!("✓ {path:?}: turn complete");
  Ok(TurnOutcome::Done)
}

// ── Streaming writes ────────────────────────────────────────────────────

/// Append-only writer that streams model deltas into the file, keeping the
/// status line at the very bottom and wrapping reasoning in thinking
/// markers.
struct StreamWriter<'a> {
  path: &'a Path,
  out: String,
  live: Option<String>,
  wrote_header: bool,
  in_reasoning: bool,
  aborted: bool,
}

impl<'a> StreamWriter<'a> {
  fn new(path: &'a Path, body: &str, live: Option<String>) -> Self {
    Self {
      path,
      out: body.to_string(),
      live,
      wrote_header: false,
      in_reasoning: false,
      aborted: false,
    }
  }

  fn ensure_header(&mut self) {
    // On resume the dangling cut already leaves the `### Assistant` header
    // in place; don't write a second one. Only a heading at the very end of
    // the buffer counts — a heading in the middle is streamed content.
    let tail = self.out.trim_end();
    if tail == "### Assistant" || tail.ends_with("\n### Assistant") {
      self.wrote_header = true;
      if !self.out.ends_with('\n') {
        self.out.push('\n');
      }
      return;
    }
    if !self.wrote_header {
      if !self.out.is_empty() && !self.out.ends_with('\n') {
        self.out.push('\n');
      }
      self.out.push_str("### Assistant\n");
      self.wrote_header = true;
      self.flush();
    }
  }

  /// Handle one stream event. Returns `false` when the turn should stop
  /// (an abort marker appeared in the file while streaming).
  fn handle(&mut self, ev: &types::StreamEvent) -> bool {
    use types::StreamEvent as E;
    if self.aborted {
      return false;
    }
    // The file is rewritten wholesale on every flush, so an abort marker
    // the user appended would be destroyed before we ever see it — check
    // the disk first.
    if self.marker_present() {
      self.aborted = true;
      self.flush_marker_strip();
      return false;
    }
    match ev {
      E::Text(t) => {
        self.ensure_header();
        if self.in_reasoning {
          self.in_reasoning = false;
          self.out.push_str("<!-- /thinking -->\n");
        }
        self.out.push_str(t);
        self.flush_marker_strip();
      }
      E::Reasoning(r) => {
        self.ensure_header();
        if !self.in_reasoning {
          self.in_reasoning = true;
          self.out.push_str("<!-- thinking -->\n");
        }
        self.out.push_str(r);
        self.flush_marker_strip();
      }
      E::ToolCallFragment(_) => { /* tool block written after completion */ }
      E::Done(_) => {}
    }
    !self.aborted
  }

  /// True while no abort marker is in the file (continue streaming). The
  /// marker must be its own line (or glued to the status line) — prose that
  /// merely mentions the marker must not stop the stream.
  fn marker_present(&self) -> bool {
    let Ok(disk) = std::fs::read_to_string(self.path) else {
      return true;
    };
    transcript::abort_marker_present(&disk)
  }

  /// Flush, dropping any abort marker line from the disk content (it is
  /// daemon-owned control state, not user text).
  fn flush_marker_strip(&mut self) {
    if self.marker_present() {
      self.aborted = true;
      self.out = without_abort_lines(&self.out);
    }
    self.flush();
  }

  fn finish_section(&self) {
    // Ensure the file ends with the section footer even if flushed already;
    // this is a no-op if the last flush already carried everything.
    let (body, _) = strip_live_line(&self.out);
    let mut out = body;
    if self.in_reasoning {
      out.push_str("<!-- /thinking -->\n");
    }
    if !out.ends_with('\n') {
      out.push('\n');
    }
    if let Some(live) = &self.live {
      out.push('\n');
      out.push_str(live);
    }
    let clean = without_abort_lines(&out);
    let _ = std::fs::write(self.path, clean);
  }

  fn flush(&mut self) {
    // Daemon-owned control lines (e.g. abort marker) never reach the file.
    let mut out = without_abort_lines(&self.out);
    if let Some(live) = &self.live {
      out.push('\n');
      out.push_str(live);
      out.push('\n');
    }
    if let Err(e) = std::fs::write(self.path, out) {
      warn!("stream write failed: {e}");
    }
  }
}

// ── Session init ────────────────────────────────────────────────────────

/// Daemon-owned comment lines never reach the LLM and are never treated as
/// user drafts. Written below `@done` when the model hits the token cap
/// mid-response; the user trims old turns and writes the next message below
/// it.
pub const CONTEXT_FULL_NOTE: &str = "<!-- slopfile:context-full: the model hit the token cap mid-response; trim the useless old turns above, then write your next message below this note -->";

/// `slopfile init <path>`: write the default template into a new or empty
/// file. Refuses to overwrite anything with content.
fn cmd_init(path: &str, cfg: &AppConfig) {
  match init_template_for(path, cfg) {
    Ok(content) => match std::fs::write(path, content) {
      Ok(()) => println!("initialized {path}"),
      Err(e) => {
        eprintln!("slopfile init: cannot write {path}: {e}");
        std::process::exit(1);
      }
    },
    Err(e) => {
      eprintln!("slopfile init: {e}");
      std::process::exit(1);
    }
  }
}

/// Resolve what `init` would write. Err when the file exists with content.
fn init_template_for(path: &str, cfg: &AppConfig) -> Result<String, String> {
  if let Ok(existing) = std::fs::read_to_string(path)
    && !existing.trim().is_empty()
  {
    return Err(format!("{path} exists and is not empty; refusing to overwrite"));
  }
  Ok(session_template(cfg))
}

/// `slopfile abort <path>`: append the abort marker to a session file. The
/// daemon notices it on the next sweep and stops the running stream.
fn cmd_abort(path: &str) {
  let content = match std::fs::read_to_string(path) {
    Ok(c) => c,
    Err(e) => {
      eprintln!("slopfile abort: cannot read {path}: {e}");
      std::process::exit(1);
    }
  };
  if transcript::abort_marker_present(&content) {
    println!("abort already requested on {path}");
    return;
  }
  let mut out = content;
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(transcript::ABORT_MARKER);
  out.push('\n');
  if let Err(e) = std::fs::write(path, out) {
    eprintln!("slopfile abort: cannot write {path}: {e}");
    std::process::exit(1);
  }
  println!("abort requested on {path}; the daemon stops the turn on its next sweep");
}

/// The default session file: header (default settings) + system prompt
/// (adapted from pi) + an empty `### You` section for the first message.
pub fn session_template(cfg: &AppConfig) -> String {
  let mut out = String::new();
  out.push_str("```toml\n");
  out.push_str(&format!("model = \"{}\"\n", cfg.model));
  out.push_str("thinking = \"auto\"\n");
  out.push_str("tools = [\"read\", \"write\", \"edit\", \"bash\", \"grep\", \"find\", \"ls\"]\n");
  out.push_str("```\n");
  out.push_str("# Session\n");
  out.push_str(&system_prompt_default(cfg));
  out.push_str("\n### You\n");
  out
}

/// The default system prompt, adapted from pi's `default_system_prompt`
/// (pi_agent_rust/src/app.rs) to slopfile's tool set and file-as-UI format.
fn system_prompt_default(cfg: &AppConfig) -> String {
  format!(
    "You are an expert coding assistant operating inside slopfile, a file-as-UI coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.

This session is a Markdown transcript file:
- Your replies are streamed under `### Assistant` headings.
- Tool calls and results appear as `<!-- tool:NAME ARGS -->` / `<!-- result:NAME ok=... -->` blocks.
- A completed turn ends with `<!-- @done -->`; anything below the last `@done` is the next turn's draft.
- When you need the user to make a choice, call the `question` tool instead of guessing.

Available tools:
- read: Read files as numbered lines
- write: Create or overwrite files
- edit: Replace the first occurrence of a text fragment in a file
- bash: Execute shell commands (ls, grep, find, etc.) in the workdir
- grep: Search file contents for a substring
- find: Find files by name suffix pattern
- ls: List directory contents

Guidelines:
- Prefer grep/find/ls tools over bash for file exploration
- Use read to examine files before editing. You must use this tool instead of cat or sed.
- Use edit for precise changes (old text must match exactly)
- Use write only for new files or complete rewrites
- When summarizing your actions, output plain text directly - do NOT use cat or bash to display what you did
- Be concise in your responses
- Show file paths clearly when working with files

Current working directory: {}",
    cfg.workdir
  )
}

// ── Conversation truncation ─────────────────────────────────────────────

/// Rough char-per-token estimate for budget checks (English ≈ 4 chars/tok).
const CHARS_PER_TOKEN: usize = 4;

fn msg_chars(m: &ChatMessage) -> usize {
  m.content.as_ref().map_or(0, |c| c.len())
    + m.reasoning_content.as_ref().map_or(0, |c| c.len())
    + m
      .tool_calls
      .as_ref()
      .map_or(0, |t| t.iter().map(|c| c.arguments.len() + 16).sum())
    + m.name.as_ref().map_or(0, |n| n.len())
}

/// Drop the OLDEST messages (after the system prompt) until the estimated
/// prompt size fits `max_tokens`. Keeps the NEWEST turns. The first kept
/// message is a `User` or `Assistant` turn start — never a `Tool` result,
/// which would orphan its tool-call id. If even the newest turn doesn't
/// fit, the newest messages are kept anyway (a partial context beats an
/// empty one).
pub fn truncate_messages(messages: Vec<ChatMessage>, max_tokens: u32) -> Vec<ChatMessage> {
  if messages.is_empty() {
    return messages;
  }
  let budget = (max_tokens as usize).saturating_mul(CHARS_PER_TOKEN);
  let total: usize = messages.iter().map(msg_chars).sum();
  if total <= budget || messages.len() <= 2 {
    return messages;
  }
  // Largest prefix (1-based index) that fits within the budget.
  let sys_len = msg_chars(&messages[0]);
  let mut keep_through = messages.len() - 1;
  let mut sum = sys_len + messages.iter().skip(1).map(msg_chars).sum::<usize>();
  for (i, m) in messages.iter().enumerate().skip(1) {
    sum -= msg_chars(m);
    if sum <= budget {
      keep_through = i;
      break;
    }
  }
  let mut keep_from = keep_through;
  // Walk forward to a clean turn start but never past the last message:
  // dropping a trailing partial turn is better than orphaning tool rounds.
  while keep_from + 1 < messages.len() && !is_turn_start(&messages[keep_from]) {
    keep_from += 1;
  }
  // Even the newest message alone exceeds the budget: the forward walk can
  // not advance past the end, so an orphaned Tool tail would be kept with
  // no caller. Unwind backward to the last turn start instead.
  if keep_from + 1 == messages.len() && !is_turn_start(&messages[keep_from]) {
    while keep_from > 0 && !is_turn_start(&messages[keep_from]) {
      keep_from -= 1;
    }
  }
  if keep_from > 1 {
    let mut out = Vec::with_capacity(messages.len() - keep_from + 1);
    out.push(messages[0].clone());
    out.extend(messages[keep_from..].iter().cloned());
    return out;
  }
  messages
}

/// A message is a valid turn start after truncation: `User` always, or a
/// `Tool` result when its calling assistant AND the assistant's own full
/// round are included (handled by the caller's forward scan). `Assistant`
/// is always safe (its tool result, if any, follows after).
fn is_turn_start(m: &ChatMessage) -> bool {
  matches!(m.role, Role::User | Role::Assistant)
}

// ── Tool execution ──────────────────────────────────────────────────────

/// Execute each tool call and append its block at the end of the file
/// (below the last `@done`, preserving the status line at the bottom).
async fn write_tool_blocks(
  path: &Path,
  calls: &[ToolCall],
  cfg: &AppConfig,
  pf: &ParsedFile,
) -> Result<(), String> {
  let text = read_file(path)?;
  let (body, live) = strip_live_line(&text);

  let mut block = String::new();
  for call in calls {
    let started = std::time::Instant::now();
    // Per-session header override, else the app-wide timeout.
    let timeout = pf.config.tool_timeout_ms.unwrap_or(cfg.tool_timeout_ms);
    let result = tools::run(
      &call.name,
      &call.arguments,
      PathBuf::from(&cfg.workdir),
      timeout,
    )
    .await;
    let ms = started.elapsed().as_millis();
    block.push_str(&format!("<!-- tool:{} {} -->\n", call.name, call.arguments));
    block.push_str(&result.output);
    if !result.output.ends_with('\n') {
      block.push('\n');
    }
    let status = if result.ok { "ok" } else { "err" };
    block.push_str(&format!("<!-- result:{} {}={}ms -->\n", call.name, status, ms));
  }

  let mut out = body;
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(&block);
  if let Some((_, live)) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)
}

/// Render a `❓ QUESTIONS` block in the tail from a `question` tool call,
/// stashing the tool-call JSON so the answer can be resolved into a normal
/// tool result later.
fn render_questions_block(path: &Path, calls: &[ToolCall]) -> Result<(), String> {
  let call = calls
    .iter()
    .find(|c| c.name == "question")
    .expect("question call");
  let args: serde_json::Value = serde_json::from_str(&call.arguments)
    .map_err(|e| format!("bad question args json: {e}"))?;
  let data = args
    .get("data")
    .and_then(|d| d.as_array())
    .ok_or("question tool needs a data array")?;

  let mut qs = Vec::new();
  let mut stash = Vec::new();
  for (i, item) in data.iter().enumerate() {
    let question = item
      .get("question")
      .and_then(|q| q.as_str())
      .unwrap_or("?")
      .to_string();
    let answers = item
      .get("answers")
      .and_then(|a| a.as_array())
      .map(|a| {
        a.iter()
          .map(|x| x.as_str().unwrap_or("").to_string())
          .collect::<Vec<_>>()
      })
      .unwrap_or_default();
    let default = item
      .get("default")
      .and_then(|d| d.as_str())
      .map(String::from);
    qs.push(questions::Question {
      id: (i + 1) as u32,
      question,
      answers,
      default,
    });
    stash.push(item.clone());
  }

  let text = read_file(path)?;
  let (body, live) = strip_live_line(&text);
  let mut out = body;
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(&questions::render_block(&qs));
  // Stash each question's original JSON as a comment (recoverable from the
  // file alone).
  for (q, json) in qs.iter().zip(stash.iter()) {
    out.push_str(&format!("<!-- q:{} {} -->\n", q.id, json));
  }
  out.push_str(&format!(
    "<!-- call:{} {} -->\n",
    call.name, call.arguments
  ));
  if let Some((_, live)) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)?;
  Ok(())
}

// ── Questions resolution ────────────────────────────────────────────────

/// Can the pending questions be resolved right now? In ask mode: the user
/// must have written answers below `> ` or typed `/skip`. In yolo mode:
/// always (defaults or the first option).
///
/// Only the FIRST pending block gates progress: `resolve_questions` works
/// first-to-last and re-reads the leftmost `❓ QUESTIONS` header each pass.
/// Scoping resolvability to that same leftmost block keeps the two in sync —
/// if a later block were answered while an earlier one sat unanswered,
/// `resolve_questions` would skip the leading block (never consuming it) and
/// the caller would livelock re-entering the identical file.
fn questions_resolvable(pf: &ParsedFile, cfg: &AppConfig) -> bool {
  if cfg.confirmations == "yolo" {
    return true;
  }
  match questions_block_region(&pf.tail) {
    Some(block) => user_answers_from_tail(block).is_some() || has_skip(block),
    None => false,
  }
}

/// The text of the leftmost QUESTIONS block in `tail`: from the `❓ QUESTIONS`
/// header through the end of its `<!-- call: ... -->` line.
fn questions_block_region(tail: &str) -> Option<&str> {
  let header_ix = tail.find(questions::BLOCK_HEADER)?;
  let end_ix = tail[header_ix..]
    .find("<!-- call:")
    .and_then(|ix| {
      let line_start = header_ix + ix;
      tail[line_start..].find('\n').map(|nl| line_start + nl + 1)
    })
    .unwrap_or(tail.len());
  Some(&tail[header_ix..end_ix])
}

/// True if the tail contains a `/skip` line (answer questions with
/// defaults / nothing, don't wait for the user). Matches both `/skip` and
/// `> /skip` (the user types on the `>` answer line).
fn has_skip(tail: &str) -> bool {
  lex(tail).into_iter().any(|t| {
    t.kind == TokKind::Skip || (t.kind == TokKind::Answer && t.payload == "/skip")
  })
}

/// Extract the raw answer text the user typed below the `>` marker of a
/// QUESTIONS block in `tail`.
fn user_answers_from_tail(tail: &str) -> Option<String> {
  lex(tail)
    .into_iter()
    .find(|t| t.kind == TokKind::Answer && !t.payload.is_empty())
    .map(|t| t.payload.to_string())
}

/// Replace an answered QUESTIONS block with a normal tool block so the next
/// generation turn sees it as plain history.
fn resolve_questions(
  path: &Path,
  text: &str,
  pf: &ParsedFile,
  cfg: &AppConfig,
) -> Result<(), String> {
  let (body, live) = strip_live_line(text);
  let mut out = body.clone();

  for block in &pf.pending_questions {
    let Some(header_ix) = out.find(questions::BLOCK_HEADER) else {
      continue;
    };
    // The block runs until after the `<!-- q:... -->` stash comments and the
    // `<!-- call:... -->` line, so none of the block survives as stray
    // assistant text after resolution.
    let rest = &out[header_ix..];
    let end_ix = rest
      .find("<!-- call:")
      .map(|ix| {
        let line_start = header_ix + ix;
        out[line_start..]
          .find('\n')
          .map(|nl| line_start + nl + 1)
          .unwrap_or(out.len())
      })
      .unwrap_or(out.len());
    let block_text = &out[header_ix..end_ix];

    // Answers: user text below `>`, else `/skip` (default/default=empty),
    // else defaults/first (yolo). In ask mode an unanswered block stays
    // pending — an answer or /skip elsewhere in the tail (another block)
    // must never auto-answer THIS block.
    let skipping = has_skip(block_text);
    let raw = user_answers_from_tail(block_text);
    let answers = match (&raw, skipping) {
      (_, true) => block
        .questions
        .iter()
        .map(|q| (q.id, q.default.clone().unwrap_or_default()))
        .collect::<Vec<_>>(),
      (Some(t), false) => parse_answers(block, t)?,
      // yolo: default or first option per question.
      (None, false) if cfg.confirmations == "yolo" => block
        .questions
        .iter()
        .map(|q| {
          let ans = q
            .default
            .clone()
            .or_else(|| q.answers.first().cloned())
            .unwrap_or_default();
          (q.id, ans)
        })
        .collect::<Vec<_>>(),
      // ask: not answered, not skipped — keep waiting for the user. A
      // leading unanswered block stalls later blocks (they resolve in file
      // order), so stop: this pass has already flushed any earlier resolved
      // blocks via `out`, and the caller re-checks the leftmost block.
      (None, false) => break,
    };

    // Re-read the stash comment for the full tool call.
    let call_json = out
      .lines()
      .find_map(|l| {
        let l = l.trim();
        let inner = l.strip_prefix("<!-- call:")?.strip_suffix("-->")?.trim();
        Some(inner.to_string())
      })
      .unwrap_or_else(|| "{}".to_string());
    let (name, args) = call_json.split_once(' ').unwrap_or(("question", call_json.as_str()));

    let mut tool_out = "\n".to_string();
    tool_out.push_str(&format!("<!-- tool:{name} {args} -->\n"));
    let payload = serde_json::json!({
      "answers": answers.iter().map(|(id, a)| serde_json::json!({"id": id, "answer": a})).collect::<Vec<_>>()
    });
    tool_out.push_str(&payload.to_string());
    tool_out.push('\n');
    tool_out.push_str(&format!("<!-- result:{name} ok=ask -->\n"));

    let mut updated = out.clone();
    updated.replace_range(header_ix..end_ix.min(updated.len()), &tool_out);
    // Drop stray /skip lines (below the block / typed on their own line);
    // they are commands, not user messages.
    out = drop_skip_lines(&updated);
  }

  if let Some((_, live)) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)?;
  Ok(())
}

// ── Draft promotion ─────────────────────────────────────────────────────

/// Insert a `### You` header before drafted tail text (below the last
/// `@done` marker) so it becomes history. Daemon-owned comment lines
/// (e.g. the context-full note) are skipped and stay above the section.
fn promote_draft(path: &Path, text: &str) -> Result<(), String> {
  let (body, live) = strip_live_line(text);
  let done_ix = transcript::find_done_marker(&body)
    .map(|ix| ix + DONE_MARKER.len())
    .unwrap_or(0);

  // Byte length of the leading blank + daemon-owned comment lines.
  let rest = &body[done_ix..];
  let skip = rest
    .lines()
    .take_while(|l| l.trim().is_empty() || lex_line(l).is_daemon_owned())
    .map(|l| l.len() + 1)
    .sum::<usize>()
    .min(rest.len());

  // Insert `### You\n` right after the skipped lines.
  let marker_end = done_ix + skip;
  let mut out = String::new();
  out.push_str(&body[..marker_end]);
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str("### You\n");
  out.push_str(body[marker_end..].trim_start_matches('\n'));
  if let Some((_, live)) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)
}

/// Tail text below the last `@done` that is NOT a questions block, NOT an
/// existing section header (`### You` / `### Assistant`), and NOT a
/// daemon-owned comment — i.e. user draft to promote. Returns None when
/// there is no draft.
fn draft_tail_after_done(text: &str) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  let done = lex(&body).into_iter().find(|t| t.kind == TokKind::Done)?;
  let tail = &body[done.offset + lex::DONE_MARKER.len()..];
  let toks = lex(tail);
  let user_toks: Vec<_> = toks.iter().filter(|t| !t.kind.is_daemon_owned()).collect();
  // Already stays in history: the tail starts with an EXACT `### You` or
  // `### Assistant` section heading (daemon promoted, or an in-flight turn).
  // Any other heading (`#`, `##`, `####` ...) is ordinary draft text and
  // must be promoted — otherwise such drafts are silently never sent.
  if user_toks
    .iter()
    .find(|t| !t.text.trim().is_empty())
    .is_some_and(|t| {
      t.kind == TokKind::Heading
        && (t.text.trim() == "### You" || t.text.trim() == "### Assistant")
    })
  {
    return None;
  }
  let user_tail = user_toks
    .iter()
    .map(|t| t.text)
    .collect::<Vec<_>>()
    .join("\n");
  let user_tail = user_tail.trim();
  if user_tail.is_empty() {
    return None;
  }
  // If the tail is ONLY the QUESTIONS block (and its stash comments), it is
  // not a draft — questions are handled separately.
  if user_tail.starts_with(questions::BLOCK_HEADER) {
    return None;
  }
  Some(user_tail.to_string())
}

/// Remove `/skip` lines (on their own line or on a `>` answer line): they
/// are commands, not user messages.
fn drop_skip_lines(text: &str) -> String {
  let had_nl = text.ends_with('\n');
  let mut out = lex(text)
    .into_iter()
    .filter(|t| {
      t.kind != TokKind::Skip && !(t.kind == TokKind::Answer && t.payload == "/skip")
    })
    .map(|t| t.text)
    .collect::<Vec<_>>()
    .join("\n");
  if had_nl && !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out
}

// ── Misc helpers ────────────────────────────────────────────────────────

/// Restart the llama-server container through the docker socket (raw HTTP,
/// no docker CLI needed). Kills an abandoned generation instantly: llama.cpp
/// has no cancel endpoint and disconnect detection is not wired into the
/// generation loop, so an aborted stream would otherwise keep decoding
/// (bounded only by max_tokens) and pin the CPU for minutes.
///
/// Requires the docker socket mounted into this container
/// (`/var/run/docker.sock`, see docker-pi/docker-compose.yml) and the
/// container user in the host's docker group.
fn restart_llama_server(socket: &str, container: &str) -> Result<(), String> {
  use std::io::{Read, Write};
  use std::os::unix::net::UnixStream;

  let mut stream = UnixStream::connect(socket).map_err(|e| format!("cannot open {socket}: {e}"))?;
  stream.set_read_timeout(Some(std::time::Duration::from_secs(20))).map_err(|e| e.to_string())?;
  let req = format!(
    "POST /containers/{container}/restart HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
  );
  stream
    .write_all(req.as_bytes())
    .map_err(|e| format!("docker socket write: {e}"))?;
  let mut buf = [0u8; 1024];
  let n = stream.read(&mut buf).map_err(|e| format!("docker socket read: {e}"))?;
  let head = String::from_utf8_lossy(&buf[..n]);
  // 204 = restart accepted; the container comes back on its own (compose
  // restart: unless-stopped).
  if head.starts_with("HTTP/1.1 204") || head.starts_with("HTTP/1.1 200") {
    Ok(())
  } else {
    Err(format!(
      "docker restart {} failed: {}",
      container,
      head.lines().next().unwrap_or("empty response")
    ))
  }
}

/// True while the file on disk currently holds an abort marker. Polled
/// between SSE deltas; if the file can't be read, abort conservatively.
fn file_has_abort(path: &Path) -> bool {
  let Ok(disk) = std::fs::read_to_string(path) else {
    return true;
  };
  transcript::abort_marker_present(&disk)
}

/// Drop every abort-marker line from `text` (daemon-owned control state,
/// not user content). The single helper behind all abort-line stripping
/// (StreamWriter flushes, the idle abort path, `slopfile abort` cleanup).
fn without_abort_lines(text: &str) -> String {
  let mut out = String::new();
  for l in text.split('\n') {
    if transcript::is_abort_line(l) {
      continue;
    }
    out.push_str(l);
    out.push('\n');
  }
  out.truncate(out.len().saturating_sub(1));
  out
}

/// Remove the abort marker line from the file (control state, not user
/// text) and leave everything else untouched.
fn strip_abort_marker(path: &Path, text: &str) -> Result<(), String> {
  let (body, live) = strip_live_line(text);
  let mut out = without_abort_lines(&body);
  if let Some((_, live)) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)
}

fn read_file(path: &Path) -> Result<String, String> {
  std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

fn write_file(path: &Path, content: String) -> Result<(), String> {
  std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
}

fn write_status(path: &Path, pf: &ParsedFile, phase: &str) -> Result<(), String> {
  let text = read_file(path)?;
  let (body, _live) = strip_live_line(&text);
  let status = types::LiveStatus {
    phase: phase.to_string(),
    model: pf.config.model.clone().unwrap_or_default(),
  };
  let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".into());
  let mut out = body;
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(&format!("<!-- slopfile:live {json} -->\n"));
  write_file(path, out)?;
  Ok(())
}

#[cfg(test)]
mod main_tests;