//! slopfile daemon: watches the transcripts dir, drives each session `.md`
//! through generation turns against llama-server, streaming deltas into the
//! file. The file is the entire UI — the daemon only ever writes below the
//! last `@done` marker (or replaces daemon-owned partial output).

mod agent;
mod api;
mod questions;
mod sse;
mod tools;
mod transcript;
mod types;

use api::LlamaClient;
use questions::{parse_answers, QuestionsBlock};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};
use transcript::{DONE_MARKER, ParsedFile};
use types::{AppConfig, ChatMessage, Role, ToolCall};

/// Host path; the container variant is `/shared/slopfile.toml`.
const APP_CONFIG_PATH: &str = "/home/unitazer/things/clanker/docker-pi/shared/slopfile.toml";

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "slopfile=info,warn".into()),
    )
    .init();

  let app_cfg = AppConfig::load_from_file(APP_CONFIG_PATH).unwrap_or_else(|e| {
    warn!("could not load app config ({e}); using defaults");
    AppConfig::default()
  });
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
      {
        let th = throttled.lock().await;
        if let Some(at) = th.get(&path) {
          if at.elapsed() < Duration::from_secs(5) {
            continue;
          }
        }
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
    if let Some(draft) = draft_tail_after_done(&text) {
      if !draft.trim().is_empty() {
        promote_draft(path, &text, &pf)?;
        continue;
      }
    }

    // 3) Generation turn: cut dangling partial output, stream a response.
    let outcome = run_generation_turn(path, &text, &pf, cfg, client).await?;
    match outcome {
      TurnOutcome::Done => return Ok(()),
      TurnOutcome::AwaitsNextTurn => continue,
      TurnOutcome::ErrorStop => return Ok(()),
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
  if let Some(draft) = draft_tail_after_done(text) {
    if !draft.trim().is_empty() {
      return true;
    }
  }
  // A `### You` / other section below the last @done (user wrote their own
  // header, or promotion happened) still needs work.
  if let Some(section) = section_below_done(text) {
    if !section.trim().is_empty() {
      return true;
    }
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

/// Raw text below the last `@done` marker (status line stripped).
fn section_below_done(text: &str) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  let done = transcript::find_done_marker(&body)?;
  let tail = body[done + DONE_MARKER.len()..].trim().to_string();
  if tail.is_empty() {
    return None;
  }
  if tail.starts_with(questions::BLOCK_HEADER) {
    return None;
  }
  Some(tail)
}

enum TurnOutcome {
  Done,
  AwaitsNextTurn,
  ErrorStop,
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
  // Cut daemon-owned partial output (dangling) so we stream a fresh response.
  let (body, live) = strip_live_line(text);
  let base_text = match &pf.dangling {
    Some(d) => {
      let cut = body[..d.resume_offset.min(body.len())].to_string();
      cut
    }
    None => body.clone(),
  };

  // Messages: history + resume hint for dangling output + any in-flight
  // below-@done sections (drafts promoted to `### You`).
  let mut messages = pf.history.clone();
  if transcript::find_done_marker(text).is_some() {
    if let Some(section) = section_below_done(text) {
      for m in transcript::tail_sections(&section) {
        messages.push(m);
      }
    }
  }
  if let (true, Some(d)) = (pf.dangling.is_some(), &pf.dangling) {
    if !d.partial_text.trim().is_empty() {
      messages.push(ChatMessage::user(format!(
        "[resume] previous assistant output was interrupted; continue exactly where it stopped:\n```\n{}\n```",
        d.partial_text.trim_end()
      )));
    }
  }

  let max_tokens = pf
    .config
    .max_context_tokens
    .unwrap_or(cfg.max_context_tokens)
    / 4;
  let tools = pf.config.tools.as_ref().map(|t| !t.is_empty()).unwrap_or(true);
  let tool_defs = tools.then(tools::definitions);

  // First write: `### Assistant` header (+ thinking open if the first delta
  // is reasoning — we handle markers in the event closure).
  let mut active = StreamWriter::new(path, &base_text, live.clone());

  info!("→ {path:?}: generating (messages={})", messages.len());
  let result = client
    .chat_stream(&messages, tool_defs.as_deref(), max_tokens, |ev| {
      active.handle(&ev);
    })
    .await
    .map_err(|e| format!("chat_stream: {e}"))?;

  active.finish_section();

  if !result.tool_calls.is_empty() {
    let is_question = result.tool_calls.iter().any(|t| t.name == "question");
    if is_question {
      render_questions_block(path, &result.tool_calls, &pf)?;
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
  if let Some(live) = live {
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
}

impl<'a> StreamWriter<'a> {
  fn new(path: &'a Path, body: &str, live: Option<String>) -> Self {
    Self {
      path,
      out: body.to_string(),
      live,
      wrote_header: false,
      in_reasoning: false,
    }
  }

  fn ensure_header(&mut self) {
    // On resume the dangling cut already leaves the `### Assistant` header
    // in place; don't write a second one.
    if self.out.trim_end().ends_with("### Assistant") {
      self.wrote_header = true;
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

  fn handle(&mut self, ev: &types::StreamEvent) {
    use types::StreamEvent as E;
    match ev {
      E::Text(t) => {
        self.ensure_header();
        if self.in_reasoning {
          self.in_reasoning = false;
          self.out.push_str("<!-- /thinking -->\n");
        }
        self.out.push_str(t);
        self.flush();
      }
      E::Reasoning(r) => {
        self.ensure_header();
        if !self.in_reasoning {
          self.in_reasoning = true;
          self.out.push_str("<!-- thinking -->\n");
        }
        self.out.push_str(r);
        self.flush();
      }
      E::ToolCallFragment(_) => { /* tool block written after completion */ }
      E::Done(_) => {}
    }
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
      out.push_str("\n");
      out.push_str(live);
    }
    let _ = std::fs::write(self.path, out);
  }

  fn flush(&mut self) {
    let mut out = self.out.clone();
    if let Some(live) = &self.live {
      out.push('\n');
      out.push_str(live);
    }
    if let Err(e) = std::fs::write(self.path, out) {
      warn!("stream write failed: {e}");
    }
  }
}

// ── Tool execution ──────────────────────────────────────────────────────

/// Execute each tool call and append its block at the end of the file
/// (below the last `@done`, preserving the status line at the bottom).
async fn write_tool_blocks(
  path: &Path,
  calls: &[ToolCall],
  cfg: &AppConfig,
  _pf: &ParsedFile,
) -> Result<(), String> {
  let text = read_file(path)?;
  let (body, live) = strip_live_line(&text);

  let mut block = String::new();
  for call in calls {
    let started = std::time::Instant::now();
    let result = tools::run(
      &call.name,
      &call.arguments,
      PathBuf::from(&cfg.workdir),
      cfg.tool_timeout_ms,
    )
    .await;
    let ms = started.elapsed().as_millis();
    block.push_str(&format!("<!-- tool:{} {} -->\n", call.name, call.arguments));
    block.push_str(&result.output);
    if !result.output.ends_with('\n') {
      block.push('\n');
    }
    block.push_str(&format!("<!-- result:{} ok={}ms -->\n", call.name, ms));
  }

  let mut out = body;
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(&block);
  if let Some(live) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)
}

/// Render a `❓ QUESTIONS` block in the tail from a `question` tool call,
/// stashing the tool-call JSON so the answer can be resolved into a normal
/// tool result later.
fn render_questions_block(
  path: &Path,
  calls: &[ToolCall],
  pf: &ParsedFile,
) -> Result<(), String> {
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
  if let Some(live) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)?;
  let _ = pf;
  Ok(())
}

// ── Questions resolution ────────────────────────────────────────────────

/// Can the pending questions be resolved right now? In ask mode: the user
/// must have written answers below `> `. In yolo mode: always (defaults or
/// the first option).
fn questions_resolvable(pf: &ParsedFile, cfg: &AppConfig) -> bool {
  if cfg.confirmations == "yolo" {
    return true;
  }
  user_answers_from_tail(&pf.tail).is_some()
}

/// Cheap heuristic used only for `block_answered` — superseded by
/// `questions_resolvable` (tail-based). Kept for symmetry.
fn block_answered(_block: &QuestionsBlock) -> bool {
  false
}

/// Extract the raw answer text the user typed below the `>` marker of a
/// QUESTIONS block in `tail`.
fn user_answers_from_tail(tail: &str) -> Option<String> {
  let header = questions::BLOCK_HEADER;
  let start = tail.find(header)?;
  let rest = &tail[start..];
  // Answer line: "> " followed by the user's text (until the next line that
  // is a comment or blank-sections out).
  for line in rest.lines() {
    if let Some(ans) = line.trim_start().strip_prefix('>') {
      let ans = ans.trim();
      if !ans.is_empty() {
        return Some(ans.to_string());
      }
    }
  }
  let _ = rest;
  None
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

    // Answers: user text below `>`, else defaults/first (yolo).
    let raw = user_answers_from_tail(block_text);
    let answers = match &raw {
      Some(t) => parse_answers(block, &t).map_err(|e| e)?,
      None => {
        // yolo: default or first option per question.
        block
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
          .collect::<Vec<_>>()
      }
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
    out = updated;

    if cfg.confirmations == "yolo" && raw.is_some() {
      // fallthrough
    }
  }

  if let Some(live) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)?;
  let _ = cfg;
  Ok(())
}

// ── Draft promotion ─────────────────────────────────────────────────────

/// Insert a `### You` header before drafted tail text (below the last
/// `@done` marker) so it becomes history.
fn promote_draft(path: &Path, text: &str, pf: &ParsedFile) -> Result<(), String> {
  let (body, live) = strip_live_line(text);
  let done_ix = transcript::find_done_marker(&body)
    .map(|ix| ix + DONE_MARKER.len())
    .unwrap_or(0);

  // Insert `### You\n` right after the marker's newline.
  let marker_end = done_ix;
  let mut out = String::new();
  out.push_str(&body[..marker_end]);
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str("### You\n");
  out.push_str(&body[marker_end..].trim_start_matches('\n'));
  if let Some(live) = live {
    out.push('\n');
    out.push_str(&live);
  }
  write_file(path, out)?;
  let _ = pf;
  Ok(())
}

/// Tail text below the last `@done` that is NOT a questions block and NOT an
/// existing section header (`### You` / `### Assistant`) — i.e. user draft to
/// promote. Returns None when there is no draft.
fn draft_tail_after_done(text: &str) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  let done = transcript::find_done_marker(&body)?;
  let tail = body[done + DONE_MARKER.len()..].trim().to_string();
  if tail.is_empty() {
    return None;
  }
  // If the tail is ONLY the QUESTIONS block (and its stash comments), it is
  // not a draft — questions are handled separately.
  if tail.starts_with(questions::BLOCK_HEADER) {
    return None;
  }
  // Already stays in history: user typed `### You`, or daemon promoted and
  // the turn is running / waiting. Do NOT re-promote.
  if tail.starts_with("### ") {
    return None;
  }
  Some(tail)
}

// ── Misc helpers ────────────────────────────────────────────────────────

fn read_file(path: &Path) -> Result<String, String> {
  std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

fn write_file(path: &Path, content: String) -> Result<(), String> {
  std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
}

fn strip_live_line(text: &str) -> (String, Option<String>) {
  let mut lines: Vec<&str> = text.lines().collect();
  let live = lines
    .iter()
    .position(|l| l.trim_start().starts_with("<!-- slopfile:live"))
    .map(|ix| {
      let s = lines.remove(ix);
      s.to_string()
    });
  let body = if lines.is_empty() {
    String::new()
  } else {
    let mut b = lines.join("\n");
    if text.ends_with('\n') && !b.is_empty() {
      b.push('\n');
    }
    b
  };
  (body, live)
}

fn write_status(path: &Path, pf: &ParsedFile, phase: &str) -> Result<(), String> {
  let text = read_file(path)?;
  let (body, _live) = strip_live_line(&text);
  let status = types::LiveStatus {
    phase: phase.to_string(),
    model: pf.config.model.clone().unwrap_or_default(),
    ..Default::default()
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

/// Cheap heuristic used only for `block_answered` — replaced by real parsing
/// below (`user_answers_from_tail`). Kept for symmetry.
fn block_text_has_answers(_s: &str) -> bool {
  // Answer presence is checked via `user_answers_from_tail` in resolve; this
  // resolver simply reports "answered" if the tail for the block contains an
  // answer line. We cannot access the tail from here, so treat as always
  // "pending parse" — resolved by resolve_questions when text exists.
  false
}

#[allow(dead_code)]
fn _unused(_x: () ) {}