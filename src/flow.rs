use crate::context;
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
    model: Some(cfg.model.clone()),
    tools: Some(DEFAULT_TOOLS.iter().map(|t| t.to_string()).collect()),
    tools_excluded: excluded.to_vec(),
    ..Default::default()
  };
  Session::new(config, system_prompt_default(cfg, excluded))
}

pub fn subagent_task_template(cfg: &AppConfig, excluded: &[String], task: &str) -> Session {
  let mut session = session_template(cfg, excluded);
  session.input = format!("{}\n", task.trim_end());
  session
}

pub fn spawn_subagent(parent: &Path, task: &str, cfg: &AppConfig) -> Result<PathBuf, String> {
  let dir = parent.parent().ok_or("parent transcript has no directory")?;
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

pub fn inject_subagent_report(
  session: &mut Session,
  name: &str,
  report: &str,
  ok: bool,
) -> bool {
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

pub fn system_prompt_default(cfg: &AppConfig, tools_excluded: &[String]) -> String {
  let has = |t: &str| !tools_excluded.iter().any(|x| x == t);
  let decision_line = if has("question") {
    "When you need the user to make a decision, call the `question` tool and offer concrete, distinct options instead of guessing."
  } else {
    "There is no `question` tool and nobody will answer; make reasonable decisions yourself and state your reasoning."
  };
  let mut out = format!(
    "You are an expert software engineering assistant. You help the user by reading files, running commands, editing code, and writing files, and you reply to them in plain text.

You act by calling tools. Your text replies and tool calls are relayed to the user automatically, one step at a time; continue based on each tool result. {decision_line}

Available tools:
- read: Read files as numbered lines
- write: Create or overwrite files
- edit: Replace the first occurrence of a text fragment in a file
- bash: Execute shell commands (ls, grep, find, etc.) in the workdir
- grep: Search file contents for a substring
- find: Find files by name suffix pattern
- ls: List directory contents"
  );
  if has("subagent") {
    out.push_str("\n- subagent: Spawn a worker with its own fresh context to handle a self-contained task; its final report is returned here automatically");
  }
  out.push_str(
    "\n\nGuidelines:
- Prefer grep/find/ls tools over bash for file exploration
- Use read to examine files before editing. Use this tool instead of cat or sed.
- Use edit for precise changes (old text must match exactly)
- Use write only for new files or complete rewrites",
  );
  if has("subagent") {
    out.push_str("\n- Use subagent for large self-contained subtasks (e.g. 'research X and report back', 'fix all lint errors in module Y'); give it every detail it needs, since it cannot see this conversation. Spawn it, then wait for its report instead of duplicating its work.");
  }
  out.push_str(
    "\n- NEVER dump large file listings into the conversation. Prefer `fd` over find (it respects .gitignore, so build/generated dirs stay out of your context); scope listings to a specific subdirectory and pattern, and pipe through head -50 when output could be long.",
  );
  out.push_str(
    "\n- The container filesystem is ephemeral: anything installed via a package manager vanishes on rebuild. If a required tool or version is missing, stop and tell the user so the image gets updated; do not paper over it with ad-hoc installs.",
  );
  out.push_str(&format!(
    "\n- Be concise in your replies; don't echo back full file contents unless asked
- Show file paths clearly when working with files

Current working directory: {}",
    cfg.workdir
  ));
  out.push_str(&skills_index(&cfg.skills_dir));
  out
}

fn skills_index(dir: &str) -> String {
  let entries = match std::fs::read_dir(dir) {
    Ok(entries) => entries,
    Err(_) => return String::new(),
  };
  let mut files: Vec<PathBuf> = entries
    .flatten()
    .map(|e| e.path())
    .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
    .collect();
  files.sort();
  if files.is_empty() {
    return String::new();
  }
  let mut listed = Vec::new();
  for f in files {
    let Ok(content) = std::fs::read_to_string(&f) else {
      continue;
    };
    if frontmatter_disabled(&content) {
      continue;
    }
    let name = f
      .file_name()
      .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    match skill_summary(&content) {
      Some(desc) => listed.push(format!("- {name}: {desc}\n")),
      None => listed.push(format!("- {name}\n")),
    }
  }
  if listed.is_empty() {
    return String::new();
  }
  let mut out = format!(
    "\nSkills available in {dir}. When a task matches a skill, read that file fully and follow it:\n"
  );
  out.push_str(&listed.concat());
  out
}

fn frontmatter_disabled(content: &str) -> bool {
  let mut lines = content.lines();
  if !lines.next().is_some_and(|l| l.trim() == "---") {
    return false;
  }
  for line in lines {
    let trimmed = line.trim();
    if trimmed == "---" || trimmed == "..." {
      break;
    }
    if trimmed.starts_with("disable-model-invocation:") && trimmed.contains("true") {
      return true;
    }
  }
  false
}

fn skill_summary(content: &str) -> Option<String> {
  let lines: Vec<&str> = content.lines().collect();
  if lines.first().is_some_and(|l| l.trim() == "---") {
    frontmatter_summary(&lines[1..])
  } else {
    body_summary(&lines)
  }
}

fn frontmatter_summary(lines: &[&str]) -> Option<String> {
  let mut title: Option<String> = None;
  let mut desc_parts: Vec<String> = Vec::new();
  let mut in_desc = false;
  for line in lines {
    let trimmed = line.trim();
    if trimmed == "---" || trimmed == "..." {
      break;
    }
    if in_desc {
      if line.starts_with(' ') && !trimmed.is_empty() {
        desc_parts.push(trimmed.to_string());
        continue;
      }
      in_desc = false;
    }
    if let Some(rest) = trimmed.strip_prefix("description:") {
      in_desc = true;
      let value = rest
        .trim()
        .trim_start_matches('>')
        .trim_start_matches('|')
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
      if !value.is_empty() {
        desc_parts.push(value.to_string());
      }
    } else if let Some(rest) = trimmed.strip_prefix("name:") {
      title = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
    }
  }
  let desc = desc_parts.join(" ");
  if desc.is_empty() {
    title.filter(|t| !t.is_empty())
  } else {
    Some(truncate_chars(&desc, 160))
  }
}

fn body_summary(lines: &[&str]) -> Option<String> {
  let mut title: Option<&str> = None;
  let mut desc: Option<String> = None;
  for line in lines {
    let trimmed = line.trim();
    if trimmed.starts_with("```") {
      break;
    }
    if title.is_none() {
      if let Some(rest) = trimmed.strip_prefix("# ") {
        if !rest.is_empty() {
          title = Some(rest.trim());
        }
      }
      continue;
    }
    if trimmed.is_empty() || trimmed.starts_with('#') {
      continue;
    }
    desc = Some(truncate_chars(trimmed, 120));
    break;
  }
  desc.or(title.map(|s| s.to_string()))
}

fn truncate_chars(s: &str, max: usize) -> String {
  if s.chars().count() <= max {
    return s.to_string();
  }
  let cut: String = s.chars().take(max).collect();
  match cut.rfind(' ') {
    Some(i) => format!("{}...", &cut[..i]),
    None => format!("{cut}..."),
  }
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
  let args: serde_json::Value = serde_json::from_str(&call.arguments)
    .map_err(|e| format!("bad question args json: {e}"))?;
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

pub fn adopt_model_edit(current: &Session, path: &Path) -> Result<Option<Session>, String> {
  let text = transcript::read_file(path)?;
  if text == current.render() {
    return Ok(None);
  }
  let edited = context::adopt(&text)?;
  info!("context file edited by the model, synced: {}", path.display());
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
