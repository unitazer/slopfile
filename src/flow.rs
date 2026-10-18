use crate::context;
use crate::prompt;
use crate::questions::{self, Question};
use crate::session::{self, NoteBy, PendingQuestion, Session, ToolOutcome, Turn};
use crate::tools;
use crate::transcript;
use crate::types::{AppConfig, Confirmations, ToolCall, ToolResult};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub const SUBAGENT_SUFFIX: &str = ".sub-";

pub const DEFAULT_TOOLS: &[&str] = &[
  "read", "write", "edit", "bash", "grep", "find", "ls", "question", "subagent",
];

pub fn subagent_depth(stem: &str) -> u32 {
  stem.matches(SUBAGENT_SUFFIX).count() as u32
}

pub fn is_subagent_path(path: &Path) -> bool {
  path
    .file_stem()
    .is_some_and(|s| subagent_depth(&s.to_string_lossy()) > 0)
}

pub fn session_template(cfg: &AppConfig, excluded: &[String]) -> Session {
  let config = session::SessionConfig {
    provider: cfg.provider.clone(),
    model: Some(cfg.default_session_model.clone()),
    tools: Some(DEFAULT_TOOLS.iter().map(|t| t.to_string()).collect()),
    tools_excluded: excluded.to_vec(),
    ..Default::default()
  };
  Session::new(config, prompt::system_prompt_default(cfg, excluded))
}

pub fn subagent_task_template(cfg: &AppConfig, excluded: &[String], task: &str) -> Session {
  let mut session = session_template(cfg, excluded);
  session.input = format!("{}\n", task.trim_end());
  session
}

pub fn spawn_subagent(parent: &Path, task: &str, cfg: &AppConfig) -> Result<PathBuf, String> {
  let dir = parent
    .parent()
    .ok_or("parent transcript has no directory")?;
  let stem = parent
    .file_stem()
    .ok_or("parent transcript has no file stem")?
    .to_string_lossy()
    .into_owned();
  let depth = subagent_depth(&stem);
  if depth >= cfg.subagent_max_depth {
    return Err(format!(
      "subagent depth limit reached (max {}, this transcript is at depth {depth})",
      cfg.subagent_max_depth
    ));
  }
  let prefix = format!("{stem}{SUBAGENT_SUFFIX}");
  let mut nn = 1u32;
  let child = loop {
    let candidate = dir.join(format!("{prefix}{nn:02}.md"));
    if !candidate.exists() {
      break candidate;
    }
    nn += 1;
  };
  let child_depth = depth + 1;
  let mut excluded = vec!["question".to_string()];
  if child_depth >= cfg.subagent_max_depth {
    excluded.push("subagent".to_string());
  }
  let session = subagent_task_template(cfg, &excluded, task);
  transcript::save(&child, &session)?;
  Ok(child)
}

pub fn report_header(name: &str, ok: bool) -> String {
  format!(
    "[system] Subagent '{}' {}. Its complete final response follows.",
    name,
    if ok { "finished" } else { "failed" }
  )
}

pub fn report_body(header: &str, report: &str) -> String {
  format!("{header}\n---\n{report}\n---")
}

pub fn has_report(session: &Session, name: &str) -> bool {
  let needle = format!("[system] Subagent '{name}' ");
  session
    .turns
    .iter()
    .filter_map(|t| t.user.as_deref())
    .any(|u| u.starts_with(&needle))
}

pub fn inject_subagent_report(session: &mut Session, name: &str, report: &str, ok: bool) -> bool {
  if has_report(session, name) {
    return false;
  }
  let header = report_header(name, ok);
  session.turns.push(Turn {
    user: Some(report_body(&header, report)),
    ..Turn::default()
  });
  true
}

pub enum ToolPass {
  Outcomes {
    outcomes: Vec<ToolOutcome>,
    solved: bool,
  },
  Question,
}

pub async fn execute_tool_calls(
  calls: &[ToolCall],
  session: &Session,
  cfg: &AppConfig,
  path: &Path,
) -> Result<ToolPass, String> {
  if calls.iter().any(|c| c.name == "question") {
    return Ok(ToolPass::Question);
  }
  let mut outcomes = Vec::new();
  let mut solved = false;
  for call in calls {
    let started = std::time::Instant::now();
    let timeout = session
      .config
      .tool_timeout_ms
      .unwrap_or(cfg.tool_timeout_ms);
    let (result, is_solve) = if !session.config.allows_tool(&call.name) {
      (
        ToolResult::err(format!("{}: tool is disabled in this session", call.name)),
        false,
      )
    } else if call.name == "subagent" {
      (handle_subagent_call(path, call, cfg), false)
    } else if call.name == "solve" {
      (handle_solve_call(call), true)
    } else {
      (
        tools::run(
          &call.name,
          &call.arguments,
          PathBuf::from(&cfg.workdir),
          timeout,
          &[PathBuf::from(&cfg.skills_dir)],
          Some(path),
        )
        .await,
        false,
      )
    };
    let ms = started.elapsed().as_millis();
    outcomes.push(ToolOutcome {
      id: call.id.clone(),
      name: call.name.clone(),
      ok: result.ok,
      ms: ms as u64,
      output: result.output,
    });
    if is_solve && result.ok {
      solved = true;
      break;
    }
  }
  Ok(ToolPass::Outcomes { outcomes, solved })
}

fn handle_subagent_call(parent: &Path, call: &ToolCall, cfg: &AppConfig) -> ToolResult {
  let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
    Ok(v) => v,
    Err(e) => return ToolResult::err(format!("subagent: bad arguments json: {e}")),
  };
  let name = args
    .get("name")
    .and_then(serde_json::Value::as_str)
    .unwrap_or("subagent");
  let Some(task) = args.get("task").and_then(serde_json::Value::as_str) else {
    return ToolResult::err("subagent: missing string argument \"task\"");
  };
  if task.trim().is_empty() {
    return ToolResult::err("subagent: task must not be empty");
  }
  match spawn_subagent(parent, task, cfg) {
    Ok(child) => ToolResult::ok(format!(
      "subagent '{name}' spawned at {}. It runs autonomously and cannot see this conversation; \
       do not read its transcript unless asked. Its complete final response will be appended \
       here automatically when it finishes; wait for it instead of redoing its work.",
      child.display()
    )),
    Err(e) => ToolResult::err(e),
  }
}

fn handle_solve_call(call: &ToolCall) -> ToolResult {
  let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
    Ok(v) => v,
    Err(e) => return ToolResult::err(format!("solve: bad arguments json: {e}")),
  };
  match args.get("summary").and_then(serde_json::Value::as_str) {
    Some(s) if !s.trim().is_empty() => ToolResult::ok(format!(
      "campaign marked as solved: {s}\nThe harness takes over from here."
    )),
    _ => ToolResult::err("solve: missing non-empty string argument \"summary\""),
  }
}

pub fn render_question(session: &mut Session, call: &ToolCall) -> Result<(), String> {
  let args: serde_json::Value =
    serde_json::from_str(&call.arguments).map_err(|e| format!("bad question args json: {e}"))?;
  let data = args
    .get("data")
    .and_then(|d| d.as_array())
    .ok_or("question tool needs a data array")?;
  let questions: Vec<Question> = data
    .iter()
    .enumerate()
    .map(|(i, item)| questions::question_from_tool_item((i + 1) as u32, item))
    .collect();
  if !session.input.trim().is_empty() {
    warn!("question call arrived with unpromoted input; replacing it with the answer block");
  }
  session.input = questions::render_block(&questions);
  // The round's ids are already the short ones the file will keep, so the waiting result and the
  // recorded question have to agree with them. Using the caller's original id leaves an orphan that
  // pairing then invents a second placeholder for.
  let current_id = session
    .last_round()
    .assistant
    .calls
    .iter()
    .find(|c| c.name == "question" && c.arguments == call.arguments)
    .map(|c| c.id.clone())
    .unwrap_or_else(|| call.id.clone());
  session.set_question(PendingQuestion {
    call: session::Call {
      id: current_id.clone(),
      name: call.name.clone(),
      arguments: call.arguments.clone(),
    },
    questions,
  });
  Ok(())
}

pub fn questions_resolvable(session: &Session, cfg: &AppConfig) -> bool {
  if cfg.confirmations == Confirmations::Yolo {
    return true;
  }
  questions::answer_text(&session.input).is_some()
}

pub fn resolve_questions(session: &mut Session, cfg: &AppConfig) -> Result<bool, String> {
  let Some(pending) = session.pending_question().cloned() else {
    return Ok(false);
  };
  let skipping = questions::is_skip(&session.input);
  let raw = questions::answer_text(&session.input);
  let answers = match (&raw, skipping) {
    (_, true) => default_answers(&pending.questions),
    (Some(text), false) => questions::parse_answers(&pending.questions, text)?,
    (None, false) if cfg.confirmations == Confirmations::Yolo => {
      default_answers(&pending.questions)
    }
    (None, false) => return Ok(false),
  };
  session.answer_question(answers)?;
  Ok(true)
}

fn default_answers(questions: &[Question]) -> Vec<(u32, String)> {
  questions
    .iter()
    .map(|q| {
      let ans = q
        .default
        .clone()
        .or_else(|| q.answers.first().cloned())
        .unwrap_or_default();
      (q.id, ans)
    })
    .collect()
}

pub fn add_context_full_note(session: &mut Session) {
  session.add_note(NoteBy::Harness, context::CONTEXT_FULL_NOTE);
}

pub fn add_reasoning_exhausted_note(session: &mut Session) {
  session.add_note(NoteBy::Harness, context::REASONING_EXHAUSTED_NOTE);
}

pub fn adopt_model_edit(current: &Session, path: &Path) -> Result<Option<Session>, String> {
  let text = transcript::read_file(path)?;
  if text == current.render() {
    return Ok(None);
  }
  let edited = context::adopt(&text)?;
  info!(
    "context file edited by the model, synced: {}",
    path.display()
  );
  Ok(Some(edited))
}

pub fn note_rollback(outcomes: &mut [ToolOutcome], why: &str) {
  if let Some(last) = outcomes.last_mut() {
    if !last.output.is_empty() && !last.output.ends_with('\n') {
      last.output.push('\n');
    }
    last
      .output
      .push_str(&format!("context edit rolled back: {why}"));
    last.ok = false;
  }
}
