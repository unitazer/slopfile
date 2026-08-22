use crate::flow::{
  inject_subagent_report, is_subagent_path, questions_resolvable, promote_draft,
  render_questions_block, resolve_questions, session_template, write_status,
  write_tool_blocks, SUBAGENT_SUFFIX,
};
use crate::lex::{ABORTED_MARKER, CONTEXT_FULL_NOTE, DONE_MARKER, REPORTED_MARKER};
use crate::stream::StreamWriter;
use crate::tokens::build_request_messages;
use crate::transcript::{
  abort_marker_present, draft_tail, file_has_abort, find_done_marker, parse_file, read_file,
  reassemble, send_marker_present, strip_abort_marker, strip_live_line, tail_user_text, write_file,
  without_abort_lines, without_send_lines, ParsedFile,
};
use crate::api::LlamaClient;
use crate::types::{AppConfig, Confirmations, Phase, Role};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

fn gen_lock() -> &'static AsyncMutex<()> {
  static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| AsyncMutex::new(()))
}

pub async fn drive_file(path: &Path, cfg: &AppConfig, client: &LlamaClient) -> Result<(), String> {
  let is_sub = is_subagent_path(path);
  let eff_cfg = if is_sub {
    let mut c = cfg.clone();
    c.confirmations = Confirmations::Yolo;
    c
  } else {
    cfg.clone()
  };
  let mut turns = 0;
  let mut sent = false;
  loop {
    turns += 1;
    if turns > 20 {
      warn!("{path:?}: giving up after {turns} turns");
      if is_sub {
        deliver_subagent_result(path, SubagentReport::FromTranscript).await?;
      }
      return Ok(());
    }

    let mut text = read_file(path)?;

    if !sent && send_marker_present(&text) {
      let (body, live) = strip_live_line(&text);
      let out = reassemble(&without_send_lines(&body), "", live.as_deref());
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

    if abort_marker_present(&text) {
      strip_abort_marker(path, &text)?;
      if is_sub {
        deliver_subagent_result(path, SubagentReport::Aborted).await?;
      }
      return Ok(());
    }

    let pf = parse_file(&text);
    if !unfinished_subagents(path)?.is_empty() {
      let _g = gen_lock().lock().await;
      write_status(path, &pf, Phase::WaitingSubagents)?;
      return Ok(());
    }
    match next_action(&pf, &text, &eff_cfg, sent) {
      Action::Idle => {
        if is_sub {
          deliver_subagent_result(path, SubagentReport::FromTranscript).await?;
        }
        return Ok(());
      }
      Action::ResolveQuestions => {
        resolve_questions(path, &text, &pf, &eff_cfg)?;
        continue;
      }
      Action::PromoteDraft => {
        promote_draft(path, &text)?;
        continue;
      }
      Action::WaitForAnswer => {
        let _g = gen_lock().lock().await;
        write_status(path, &pf, Phase::WaitingAnswer)?;
        return Ok(());
      }
      Action::Generate => {}
    }

    let outcome = run_generation_turn(path, &text, &pf, &eff_cfg, client).await?;
    match outcome {
      TurnOutcome::Done => return Ok(()),
      TurnOutcome::AwaitsNextTurn => continue,
    }
  }
}

/// What the daemon should do with a parsed transcript right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
  /// Pending questions are answerable (answers present, /skip, or yolo).
  ResolveQuestions,
  /// A draft sits below the last turn-end; promote it to a `### You` section first.
  PromoteDraft,
  /// Run a generation turn (fresh user turn or dangling assistant resume).
  Generate,
  /// Armed but the model asked something nobody answered yet.
  WaitForAnswer,
  /// Nothing to do.
  Idle,
}

/// Single source of truth for whether (and how) a transcript needs work.
///
/// Questions gate everything else: an unanswered leading block stalls even a
/// later answered one. Everything below the question gate requires the file to
/// be armed — either by a consumed send marker or by yolo mode.
pub(crate) fn next_action(pf: &ParsedFile, text: &str, cfg: &AppConfig, sent: bool) -> Action {
  // yolo auto-answers questions and never waits for an explicit send.
  let armed = sent || cfg.confirmations == Confirmations::Yolo;
  if !pf.pending_questions.is_empty() {
    return match (armed, questions_resolvable(pf, cfg)) {
      (true, true) => Action::ResolveQuestions,
      (true, false) => Action::WaitForAnswer,
      (false, _) => Action::Idle,
    };
  }
  if !armed {
    return Action::Idle;
  }
  if pf.dangling.is_some() {
    return Action::Generate;
  }
  if draft_tail(text).is_some_and(|d| !d.trim().is_empty()) {
    return Action::PromoteDraft;
  }
  if tail_user_text(text).is_some_and(|s| !s.trim().is_empty()) {
    return Action::Generate;
  }
  if find_done_marker(text).is_none()
    && pf.history.last().is_some_and(|m| matches!(m.role, Role::User | Role::Tool))
  {
    return Action::Generate;
  }
  Action::Idle
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
  let _gen = gen_lock().lock().await;
  let (body, live) = strip_live_line(text);
  let base_text = match &pf.dangling {
    Some(d) => body[..d.resume_offset.min(body.len())].to_string(),
    None => body.clone(),
  };

  let messages = build_request_messages(text, pf, cfg, client.rest()).await?.messages;

  let max_tokens = crate::tokens::context_budget(pf, cfg) / 4;
  let tools_enabled = pf.config.tools.as_ref().map(|t| !t.is_empty()).unwrap_or(true);
  let excluded = pf.config.tools_excluded.clone();
  let tool_defs = tools_enabled.then(|| {
    crate::tools::definitions()
      .into_iter()
      .filter(|d| !excluded.iter().any(|x| x == &d.function.name))
      .collect::<Vec<_>>()
  });

  let model = pf.config.model.as_deref().unwrap_or(&cfg.model);
  client.ensure_model_loaded(model).await;

  let mut active = StreamWriter::new(path, &base_text, live);

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

  active.finish_section();

  if result.aborted {
    let already_marked = read_file(path)?.trim_end().ends_with(ABORTED_MARKER);
    let extra = if already_marked {
      String::new()
    } else {
      format!("{ABORTED_MARKER}\n")
    };
    finalize_turn(path, extra)?;
    write_status(path, pf, Phase::Idle)?;
    maybe_restart_llama(cfg, path).await;
    return Ok(TurnOutcome::Done);
  }

  if result.finish_reason.as_deref() == Some("length") {
    finalize_turn(path, format!("{DONE_MARKER}\n{CONTEXT_FULL_NOTE}\n"))?;
    write_status(path, pf, Phase::Idle)?;
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

  finalize_turn(path, format!("{DONE_MARKER}\n"))?;
  write_status(path, pf, Phase::Idle)?;
  info!("✓ {path:?}: turn complete");
  Ok(TurnOutcome::Done)
}

/// Re-read the file after a finished turn, drop any abort lines that landed
/// mid-stream, append `extra`, and restore the live status line.
pub(crate) fn finalize_turn(path: &Path, extra: String) -> Result<(), String> {
  let text_now = read_file(path)?;
  let (body, live) = strip_live_line(&text_now);
  let out = reassemble(&without_abort_lines(&body), &extra, live.as_deref());
  write_file(path, out)
}

async fn maybe_restart_llama(cfg: &AppConfig, path: &Path) {
  if !cfg.abort_restart_server {
    info!("✗ {path:?}: aborted");
    return;
  }
  let container = cfg.llama_container.clone();
  let res = tokio::task::spawn_blocking(move || {
    crate::docker::restart_llama_server("/var/run/docker.sock", &container)
  })
  .await;
  match res {
    Ok(Ok(())) => info!("✗ {path:?}: aborted (llama-server restarted)"),
    Ok(Err(e)) => warn!("{path:?}: aborted but couldn't restart llama-server: {e}"),
    Err(e) => warn!("{path:?}: aborted, llama restart task failed: {e}"),
  }
}

fn final_assistant_text(pf: &ParsedFile) -> Option<String> {
  pf.history.iter().rev().find_map(|m| {
    if matches!(m.role, Role::Assistant)
      && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
    {
      m.content.clone()
    } else {
      None
    }
  })
}

fn subagent_parent_path(child: &Path) -> Result<PathBuf, String> {
  let dir = child.parent().ok_or("subagent transcript has no directory")?;
  let stem = child
    .file_stem()
    .ok_or("subagent transcript has no file stem")?
    .to_string_lossy()
    .into_owned();
  let ix = stem.rfind(SUBAGENT_SUFFIX).ok_or("not a subagent transcript")?;
  Ok(dir.join(format!("{}.md", &stem[..ix])))
}

pub(crate) fn unfinished_subagents(parent: &Path) -> Result<Vec<PathBuf>, String> {
  let dir = parent.parent().ok_or("parent transcript has no directory")?;
  let stem = parent
    .file_stem()
    .ok_or("parent transcript has no file stem")?
    .to_string_lossy()
    .into_owned();
  let prefix = format!("{stem}{SUBAGENT_SUFFIX}");
  let mut out = Vec::new();
  for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
    let p = match entry {
      Ok(e) => e.path(),
      Err(_) => continue,
    };
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
      continue;
    };
    if !name.starts_with(&prefix) || !name.ends_with(".md") {
      continue;
    }
    let suffix = name
      .trim_end_matches(".md")
      .strip_prefix(&prefix)
      .expect("name starts with prefix");
    if suffix.len() != 2 || !suffix.bytes().all(|b| b.is_ascii_digit()) {
      continue;
    }
    let Ok(text) = read_file(&p) else {
      out.push(p);
      continue;
    };
    if !text.contains(REPORTED_MARKER) {
      out.push(p);
    }
  }
  out.sort();
  Ok(out)
}

pub(crate) enum SubagentReport {
  FromTranscript,
  Aborted,
}

pub(crate) async fn deliver_subagent_result(
  path: &Path,
  outcome: SubagentReport,
) -> Result<(), String> {
  let _gen = gen_lock().lock().await;
  let text = read_file(path)?;
  if text.contains(REPORTED_MARKER) {
    return Ok(());
  }
  let name = path
    .file_stem()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| "subagent".to_string());
  let (report, ok) = match outcome {
    SubagentReport::FromTranscript => {
      let pf = parse_file(&text);
      (
        final_assistant_text(&pf)
          .unwrap_or_else(|| "(subagent produced no text output)".to_string()),
        true,
      )
    }
    SubagentReport::Aborted => ("subagent was aborted before completing".to_string(), false),
  };
  inject_subagent_report(&subagent_parent_path(path)?, &name, &report, ok)?;
  let (body, live) = strip_live_line(&text);
  write_file(
    path,
    reassemble(&body, &format!("{REPORTED_MARKER}\n"), live.as_deref()),
  )?;
  info!("→ {:?}: report delivered to parent", path);
  Ok(())
}
