
mod api;
mod lex;
mod questions;
mod sse;
mod tools;
mod transcript;
mod types;

use api::LlamaClient;
use clap::{Parser, Subcommand};
use lex::{lex, lex_line, ABORTED_MARKER, DONE_MARKER, TokKind};
use questions::parse_answers;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env::var;
use std::fs::read_to_string;
use std::fs::write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::{sleep, };
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
  Init {
    path: String,
  },
  Abort {
    path: String,
  },
  Tokens {
    path: String,
  },
}

const APP_CONFIG_PATH: &str = "/home/unitazer/things/clanker/slopfile/shared/slopfile.toml";

fn load_app_config() -> AppConfig {
  let path = var("SLOPFILE_CONFIG").unwrap_or_else(|_| APP_CONFIG_PATH.to_string());
  let mut cfg = AppConfig::load_from_file(&path).unwrap_or_else(|e| {
    warn!("could not load app config ({e}); using defaults");
    AppConfig::default()
  });
  if let Ok(v) = var("SLOPFILE_API_BASE") {
    cfg.api_base = v;
  }
  if let Ok(v) = var("SLOPFILE_TRANSCRIPTS") {
    cfg.transcripts_dir = v;
  }
  if let Ok(v) = var("SLOPFILE_WORKDIR") {
    cfg.workdir = v;
  }
  if let Ok(v) = var("SLOPFILE_ABORT_RESTART") {
    cfg.abort_restart_server = v != "0" && !v.eq_ignore_ascii_case("false");
  }
  if let Ok(v) = var("SLOPFILE_LLAMA_CONTAINER") {
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
  if let Some(Command::Tokens { path }) = cli.command {
    return cmd_tokens(&path, &app_cfg).await;
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


async fn drive_file(path: &Path, cfg: &AppConfig, client: &LlamaClient) -> Result<(), String> {
  let mut turns = 0;
  let mut sent = false;
  loop {
    turns += 1;
    if turns > 20 {
      warn!("{path:?}: giving up after {turns} turns");
      return Ok(());
    }

    let mut text = read_file(path)?;

    if !sent && transcript::send_marker_present(&text) {
      let (body, live) = strip_live_line(&text);
      let mut out = without_send_lines(&body);
      if let Some((_, live)) = live {
        out.push('\n');
        out.push_str(&live);
      }
      write_file(path, out.clone())?;
      text = out;
      sent = true;
    }

    if text.trim().is_empty() {
      if read_file(path)?.trim().is_empty() {
        write_file(path, session_template(cfg))?;
        info!("→ {path:?}: initialized empty session");
      }
      return Ok(());
    }

    if transcript::abort_marker_present(&text) {
      strip_abort_marker(path, &text)?;
      return Ok(());
    }

    let pf = transcript::parse_file(&text);
    if !has_work(&pf, &text, cfg, sent) {
      return Ok(());
    }

    if !pf.pending_questions.is_empty() {
      if questions_resolvable(&pf, cfg) {
        resolve_questions(path, &text, &pf, cfg)?;
        continue;
      }
      write_status(path, &pf, "waiting_answer")?;
      return Ok(());
    }

    if let Some(draft) = draft_tail_after_done(&text)
      && !draft.trim().is_empty()
    {
      promote_draft(path, &text)?;
      continue;
    }

    let outcome = run_generation_turn(path, &text, &pf, cfg, client).await?;
    match outcome {
      TurnOutcome::Done => return Ok(()),
      TurnOutcome::AwaitsNextTurn => continue,
    }
  }
}

fn has_work(pf: &ParsedFile, text: &str, cfg: &AppConfig, sent: bool) -> bool {
  // yolo auto-answers questions and never waits for an explicit send.
  if cfg.confirmations == "yolo" {
    return auto_has_work(pf, text, cfg);
  }
  sent && auto_has_work(pf, text, cfg)
}

fn auto_has_work(pf: &ParsedFile, text: &str, cfg: &AppConfig) -> bool {
  if pf.dangling.is_some() {
    return true;
  }
  if !pf.pending_questions.is_empty() {
    return questions_resolvable(pf, cfg);
  }
  if let Some(draft) = draft_tail_after_done(text)
    && !draft.trim().is_empty()
  {
    return true;
  }
  if let Some(section) = section_below_done(text)
    && !section.trim().is_empty()
  {
    return true;
  }
  if transcript::find_done_marker(text).is_none() {
    return pf
      .history
      .last()
      .is_some_and(|m| m.role == Role::User || m.role == Role::Tool);
  }
  false
}

fn section_below_done(text: &str) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  let done_ix = transcript::turn_end_tail_start(&body)?;
  let tail = &body[done_ix..];
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

async fn run_generation_turn(
  path: &Path,
  text: &str,
  pf: &ParsedFile,
  cfg: &AppConfig,
  client: &LlamaClient,
) -> Result<TurnOutcome, String> {
  let (body, live) = strip_live_line(text);
  let base_text = match &pf.dangling {
    Some(d) => body[..d.resume_offset.min(body.len())].to_string(),
    None => body.clone(),
  };

  let (messages, _dropped, _) = build_request_messages(text, pf, cfg).await?;

  let context_budget = pf
    .config
    .max_context_tokens
    .unwrap_or(cfg.max_context_tokens);
  let max_tokens = context_budget / 4;
  let tools = pf.config.tools.as_ref().map(|t| !t.is_empty()).unwrap_or(true);
  let tool_defs = tools.then(tools::definitions);

  let model = pf.config.model.as_deref().unwrap_or(&cfg.model);
  client.ensure_model_loaded(model).await;

  let mut active = StreamWriter::new(path, &base_text, live.map(|(_, t)| t));

  info!("→ {path:?}: generating (model={model}, messages={})", messages.len());
  let result = client
    .chat_stream(
      model,
      &messages,
      tool_defs.as_deref(),
      max_tokens,
      |ev| active.handle(&ev),
      || !file_has_abort(path),
    )
    .await
    .map_err(|e| format!("chat_stream: {e}"))?;

  if result.aborted {
    active.finish_section();
    let text_now = read_file(path)?;
    let (body, live) = strip_live_line(&text_now);
    let mut out = without_abort_lines(&body);
    if !out.ends_with('\n') {
      out.push('\n');
    }
    if !out.trim_end().ends_with(ABORTED_MARKER) {
      out.push_str(&format!("{ABORTED_MARKER}\n"));
    }
    if let Some((_, live)) = live {
      out.push('\n');
      out.push_str(&live);
    }
    write_file(path, out)?;
    write_status(path, pf, "idle")?;
    if cfg.abort_restart_server {
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

  if result.finish_reason.as_deref().is_none() {
    info!("⚠ {path:?}: turn interrupted mid-stream (no finish_reason); left dangling for resume");
    return Ok(TurnOutcome::Done);
  }

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

  fn handle(&mut self, ev: &types::StreamEvent) -> bool {
    use types::StreamEvent as E;
    if self.aborted {
      return false;
    }
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
      E::ToolCallFragment(_) => { }
      E::Done(_) => {}
    }
    !self.aborted
  }

  fn marker_present(&self) -> bool {
    let Ok(disk) = read_to_string(self.path) else {
      return true;
    };
    transcript::abort_marker_present(&disk)
  }

  fn flush_marker_strip(&mut self) {
    if self.marker_present() {
      self.aborted = true;
      self.out = without_abort_lines(&self.out);
    }
    self.flush();
  }

  fn finish_section(&self) {
    let (body, _) = strip_live_line(&self.out);
    let mut out = body;
    if self.in_reasoning {
      if !out.ends_with('\n') {
        out.push('\n');
      }
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
    let _ = write(self.path, clean);
  }

  fn flush(&mut self) {
    let mut out = without_abort_lines(&self.out);
    if let Some(live) = &self.live {
      out.push('\n');
      out.push_str(live);
      out.push('\n');
    }
    if let Err(e) = write(self.path, out) {
      warn!("stream write failed: {e}");
    }
  }
}


pub const CONTEXT_FULL_NOTE: &str = "<!-- slopfile:context-full: the model hit the token cap mid-response; trim the useless old turns above, then write your next message below this note -->";

fn cmd_init(path: &str, cfg: &AppConfig) {
  match init_template_for(path, cfg) {
    Ok(content) => match write(path, content) {
      Ok(()) => println!("initialized {path}"),
      Err(e) => {
        eprintln!("slopfile init: cannot write {path}: {e}");
        exit(1);
      }
    },
    Err(e) => {
      eprintln!("slopfile init: {e}");
      exit(1);
    }
  }
}

fn init_template_for(path: &str, cfg: &AppConfig) -> Result<String, String> {
  if let Ok(existing) = read_to_string(path)
    && !existing.trim().is_empty()
  {
    return Err(format!("{path} exists and is not empty; refusing to overwrite"));
  }
  Ok(session_template(cfg))
}

fn cmd_abort(path: &str) {
  let content = match read_to_string(path) {
    Ok(c) => c,
    Err(e) => {
      eprintln!("slopfile abort: cannot read {path}: {e}");
      exit(1);
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
  if let Err(e) = write(path, out) {
    eprintln!("slopfile abort: cannot write {path}: {e}");
    exit(1);
  }
  println!("abort requested on {path}; the daemon stops the turn on its next sweep");
}

async fn cmd_tokens(path: &str, cfg: &AppConfig) {
  let text = match read_to_string(path) {
    Ok(t) => t,
    Err(e) => {
      eprintln!("slopfile tokens: cannot read {path}: {e}");
      exit(1);
    }
  };
  let pf = transcript::parse_file(&text);
  let (messages, dropped, count) = match build_request_messages(&text, &pf, cfg).await {
    Ok(v) => v,
    Err(e) => {
      eprintln!("slopfile tokens: {e}");
      exit(1);
    }
  };
  let context_budget = pf
    .config
    .max_context_tokens
    .unwrap_or(cfg.max_context_tokens);

  let headroom = context_budget.saturating_sub(count as u32);

  println!("tokens: {path}");
  println!("  messages:    {} sent ({} oldest dropped to fit budget)", messages.len(), dropped);
  println!("  model tokens: {count}  (via llama-server /tokenize, content only)");
  println!("  context budget: {context_budget}  output allowance: {}", context_budget / 4);
  println!("  input headroom: {headroom}");
}

async fn request_token_count(
  api_base: &str,
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
  let client = reqwest::Client::new();
  let mut last_err = String::new();
  for (url, body) in [
    (format!("{api_base}/tokenize"), serde_json::json!({"content": content.clone(), "model": model})),
    (format!("{api_base}/tokenize"), serde_json::json!({"content": content.clone()})),
    (format!("{api_base}/v1/tokenize"), serde_json::json!({"content": content.clone(), "model": model})),
    (format!("{api_base}/v1/tokenize"), serde_json::json!({"content": content.clone()})),
  ] {
    let resp = match client.post(&url).json(&body).send().await {
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

fn system_prompt_default(cfg: &AppConfig) -> String {
  format!(
    "You are an expert software engineering assistant. You help the user by reading files, running commands, editing code, and writing files, and you reply to them in plain text.

You act by calling tools. Your text replies and tool calls are relayed to the user automatically, one step at a time; continue based on each tool result. When you need the user to make a decision, call the `question` tool and offer concrete, distinct options instead of guessing.

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
- Use read to examine files before editing. Use this tool instead of cat or sed.
- Use edit for precise changes (old text must match exactly)
- Use write only for new files or complete rewrites
- Be concise in your replies; don't echo back full file contents unless asked
- Show file paths clearly when working with files

Current working directory: {}",
    cfg.workdir
  )
}


async fn build_request_messages(
  text: &str,
  pf: &ParsedFile,
  cfg: &AppConfig,
) -> Result<(Vec<ChatMessage>, usize, usize), String> {
  let mut messages = pf.history.clone();
  if transcript::find_done_marker(text).is_some() {
    let (body, _live) = strip_live_line(text);
    let toks = lex(&body);
    let done = toks.iter().rfind(|t| t.kind.is_turn_end()).expect("checked above");
    let start = done.offset + done.text.len();
    let end = match &pf.dangling {
      Some(d) => d.resume_offset.min(body.len()),
      None => body.len(),
    };
    let tail = if end > start { &body[start..end] } else { "" };
    let section = lex(tail)
      .into_iter()
      .filter(|t| !t.kind.is_daemon_owned())
      .map(|t| t.text)
      .collect::<Vec<_>>()
      .join("\n")
      .trim()
      .to_string();
    if !section.is_empty() && !section.starts_with(questions::BLOCK_HEADER) {
      for m in transcript::tail_sections(&section) {
        messages.push(m);
      }
    }
  }
  if let Some(d) = &pf.dangling
    && !d.partial_text.trim().is_empty()
  {
    messages.extend(transcript::assistant_section_messages(&d.partial_text));
  }
  let context_budget = pf.config.max_context_tokens.unwrap_or(cfg.max_context_tokens);
  let before = messages.len();
  if before == 0 {
    return Ok((messages, 0, 0));
  }
  let counts = request_token_counts(&cfg.api_base, pf.config.model.as_deref().unwrap_or(&cfg.model), &messages).await?;
  let truncated = truncate_messages(messages, &counts, context_budget);
  let dropped = before - truncated.len();
  let keep_from = before - (truncated.len() - 1);
  let sent_tokens = counts[0] + counts[keep_from..].iter().sum::<usize>();
  Ok((truncated, dropped, sent_tokens))
}

async fn request_token_counts(
  api_base: &str,
  model: &str,
  messages: &[ChatMessage],
) -> Result<Vec<usize>, String> {
  let mut counts = Vec::with_capacity(messages.len());
  for m in messages {
    counts.push(request_token_count(api_base, model, std::slice::from_ref(m)).await?);
  }
  Ok(counts)
}

#[cfg(test)]
fn msg_chars(m: &ChatMessage) -> usize {
  m.content.as_ref().map_or(0, |c| c.len())
    + m.reasoning_content.as_ref().map_or(0, |c| c.len())
    + m
      .tool_calls
      .as_ref()
      .map_or(0, |t| t.iter().map(|c| c.arguments.len() + 16).sum())
    + m.name.as_ref().map_or(0, |n| n.len())
}

pub fn truncate_messages(
  messages: Vec<ChatMessage>,
  token_counts: &[usize],
  max_tokens: u32,
) -> Vec<ChatMessage> {
  if messages.is_empty() {
    return messages;
  }
  let budget = max_tokens as usize;
  let total: usize = token_counts.iter().sum();
  if total <= budget || messages.len() <= 2 {
    return messages;
  }
  let sys_tokens = token_counts[0];
  let mut keep_through = messages.len() - 1;
  let mut sum = sys_tokens + token_counts[1..].iter().sum::<usize>();
  for (i, &toks) in token_counts.iter().enumerate().skip(1) {
    sum -= toks;
    if sum <= budget {
      keep_through = i;
      break;
    }
  }
  let mut keep_from = keep_through;
  while keep_from + 1 < messages.len() && !is_turn_start(&messages[keep_from]) {
    keep_from += 1;
  }
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

fn is_turn_start(m: &ChatMessage) -> bool {
  matches!(m.role, Role::User | Role::Assistant)
}


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


fn questions_resolvable(pf: &ParsedFile, cfg: &AppConfig) -> bool {
  if cfg.confirmations == "yolo" {
    return true;
  }
  match questions_block_region(&pf.tail) {
    Some(block) => user_answers_from_tail(block).is_some() || has_skip(block),
    None => false,
  }
}

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

fn has_skip(tail: &str) -> bool {
  lex(tail).into_iter().any(|t| {
    t.kind == TokKind::Skip || (t.kind == TokKind::Answer && t.payload == "/skip")
  })
}

fn user_answers_from_tail(tail: &str) -> Option<String> {
  lex(tail)
    .into_iter()
    .find(|t| t.kind == TokKind::Answer && !t.payload.is_empty())
    .map(|t| t.payload.to_string())
}

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

    let skipping = has_skip(block_text);
    let raw = user_answers_from_tail(block_text);
    let answers = match (&raw, skipping) {
      (_, true) => block
        .questions
        .iter()
        .map(|q| (q.id, q.default.clone().unwrap_or_default()))
        .collect::<Vec<_>>(),
      (Some(t), false) => parse_answers(block, t)?,
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
      (None, false) => break,
    };

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
    out = drop_skip_lines(&updated);
  }

  if let Some((_, live)) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)?;
  Ok(())
}


fn promote_draft(path: &Path, text: &str) -> Result<(), String> {
  let (body, live) = strip_live_line(text);
  let done_ix = transcript::turn_end_tail_start(&body).unwrap_or(0);

  let rest = &body[done_ix..];
  let skip = rest
    .lines()
    .take_while(|l| l.trim().is_empty() || lex_line(l).is_daemon_owned())
    .map(|l| l.len() + 1)
    .sum::<usize>()
    .min(rest.len());

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

fn draft_tail_after_done(text: &str) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  let done = lex(&body).into_iter().find(|t| t.kind.is_turn_end())?;
  let tail = &body[done.offset + done.text.len()..];
  let toks = lex(tail);
  let user_toks: Vec<_> = toks.iter().filter(|t| !t.kind.is_daemon_owned()).collect();
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
  if user_tail.starts_with(questions::BLOCK_HEADER) {
    return None;
  }
  Some(user_tail.to_string())
}

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


fn restart_llama_server(socket: &str, container: &str) -> Result<(), String> {
  use std::io::{Read, Write};
  use std::os::unix::net::UnixStream;

  let mut stream = UnixStream::connect(socket).map_err(|e| format!("cannot open {socket}: {e}"))?;
  stream.set_read_timeout(Some(Duration::from_secs(20))).map_err(|e| e.to_string())?;
  let req = format!(
    "POST /containers/{container}/restart HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
  );
  stream
    .write_all(req.as_bytes())
    .map_err(|e| format!("docker socket write: {e}"))?;
  let mut buf = [0u8; 1024];
  let n = stream.read(&mut buf).map_err(|e| format!("docker socket read: {e}"))?;
  let head = String::from_utf8_lossy(&buf[..n]);
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

fn file_has_abort(path: &Path) -> bool {
  let Ok(disk) = read_to_string(path) else {
    return true;
  };
  transcript::abort_marker_present(&disk)
}

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

fn without_send_lines(text: &str) -> String {
  let mut out = String::new();
  for l in text.split('\n') {
    if transcript::is_send_line(l) {
      continue;
    }
    out.push_str(l);
    out.push('\n');
  }
  out.truncate(out.len().saturating_sub(1));
  out
}

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
  read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

fn write_file(path: &Path, content: String) -> Result<(), String> {
  write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
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
