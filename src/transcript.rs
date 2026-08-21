
use crate::lex::{lex, lex_line, Token, TokKind};
use crate::questions::QuestionsBlock;
use crate::types::{ChatMessage, Role, SessionConfig, ToolCall};
use std::path::Path;

#[allow(unused_imports)]
pub use crate::lex::{ABORT_MARKER, SEND_MARKER};

pub fn send_marker_present(text: &str) -> bool {
  lex(text).iter().any(|t| t.kind == TokKind::Send)
}

pub fn is_send_line(line: &str) -> bool {
  lex_line(line) == TokKind::Send
}

pub fn abort_marker_present(text: &str) -> bool {
  lex(text).iter().any(|t| t.kind == TokKind::Abort)
}

pub fn is_abort_line(line: &str) -> bool {
  lex_line(line) == TokKind::Abort
}

pub fn strip_live_line(text: &str) -> (String, Option<String>) {
  let toks = lex(text);
  let mut last = None;
  let mut body = String::new();
  for tok in toks.iter() {
    if tok.kind == TokKind::Live {
      last = Some(tok.text.to_string());
      continue;
    }
    body.push_str(tok.text);
    if tok.eol_len >= 2 {
      body.push_str("\r\n");
    } else {
      body.push('\n');
    }
  }
  if last.is_none() {
    return (text.to_string(), None);
  }
  if body.ends_with('\n') {
    body.pop();
    if body.ends_with('\r') {
      body.pop();
      body.push_str("\r\n");
    } else {
      body.push('\n');
    }
  } else {
    body.push('\n');
  }
  (body, last)
}

/// Rebuild a transcript after stripping the live line: `extra` is appended to
/// `body`, then the live status line goes back at the very bottom.
pub fn reassemble(body: &str, extra: &str, live: Option<&str>) -> String {
  let mut out = body.to_string();
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(extra);
  if let Some(live) = live {
    if !out.ends_with('\n') {
      out.push('\n');
    }
    out.push_str(live);
  }
  out
}


#[derive(Debug, Clone)]
pub struct DanglingTail {
  pub resume_offset: usize,
  pub partial_text: String,
}

#[derive(Debug, Clone)]
pub struct ParsedFile {
  pub config: SessionConfig,
  pub history: Vec<ChatMessage>,
  pub tail: String,
  pub pending_questions: Vec<QuestionsBlock>,
  pub dangling: Option<DanglingTail>,
}


fn tool_call_id(name: &str, arguments: &str) -> String {
  fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
      hash ^= b as u64;
      hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
  }
  format!("{:016x}", fnv1a64(&format!("{name}{arguments}")))
}

pub fn header_config(text: &str) -> SessionConfig {
  let toks = lex(text);
  let Some(open) = toks.iter().find(|t| t.kind == TokKind::FenceOpen && t.payload == "toml") else {
    return SessionConfig::default();
  };
  let mut body = Vec::new();
  for t in toks.iter().skip(open.line + 1) {
    if t.kind == TokKind::FenceClose {
      break;
    }
    body.push(t.text);
  }
  let body = body.join("\n");
  if body.trim().is_empty() {
    return SessionConfig::default();
  }
  toml::from_str(&body).unwrap_or_default()
}

pub fn find_done_marker(text: &str) -> Option<usize> {
  lex(text).iter().rfind(|t| t.kind.is_turn_end()).map(|t| t.offset)
}

pub fn turn_end_tail_start(text: &str) -> Option<usize> {
  lex(text)
    .iter()
    .rfind(|t| t.kind.is_turn_end())
    .map(|t| t.offset + t.text.len())
}

/// User-authored text below the last turn-end marker (draft, pending request).
/// Daemon-owned lines and QUESTIONS blocks are excluded. `None` = empty tail.
pub fn tail_user_text(text: &str) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  user_text_below_done(&body)
}

/// Same as `tail_user_text`, but only text strictly before byte offset `cap`
/// counts (used to exclude a dangling partial assistant section).
pub fn tail_user_text_capped(text: &str, cap: usize) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  let end = cap.min(body.len());
  user_text_below_done(&body[..end])
}

fn user_text_below_done(body: &str) -> Option<String> {
  let start = turn_end_tail_start(body)?;
  let joined = lex(&body[start..])
    .into_iter()
    .filter(|t| !t.kind.is_daemon_owned())
    .map(|t| t.text)
    .collect::<Vec<_>>()
    .join("\n")
    .trim()
    .to_string();
  if joined.is_empty() || joined.starts_with(crate::questions::BLOCK_HEADER) {
    return None;
  }
  Some(joined)
}

/// Like `tail_user_text`, but a tail that opens with an explicit section
/// heading is not a draft (it is already a structured turn).
pub fn draft_tail(text: &str) -> Option<String> {
  let (body, _live) = strip_live_line(text);
  let done = lex(&body).into_iter().find(|t| t.kind.is_turn_end())?;
  let toks = lex(&body[done.offset + done.text.len()..]);
  let user: Vec<&Token<'_>> = toks.iter().filter(|t| !t.kind.is_daemon_owned()).collect();
  if user
    .iter()
    .find(|t| !t.text.trim().is_empty())
    .is_some_and(|t| {
      t.kind == TokKind::Heading && matches!(t.text.trim(), "### You" | "### Assistant")
    })
  {
    return None;
  }
  let joined = user
    .iter()
    .map(|t| t.text)
    .collect::<Vec<_>>()
    .join("\n")
    .trim()
    .to_string();
  if joined.is_empty() || joined.starts_with(crate::questions::BLOCK_HEADER) {
    return None;
  }
  Some(joined)
}


pub fn read_file(path: &Path) -> Result<String, String> {
  std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

pub fn write_file(path: &Path, content: String) -> Result<(), String> {
  std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn file_has_abort(path: &Path) -> bool {
  let Ok(disk) = read_file(path) else {
    return true;
  };
  abort_marker_present(&disk)
}

fn without_lines_matching(text: &str, drop: impl Fn(&str) -> bool) -> String {
  let mut out = String::new();
  for l in text.split('\n') {
    if drop(l) {
      continue;
    }
    out.push_str(l);
    out.push('\n');
  }
  out.truncate(out.len().saturating_sub(1));
  out
}

pub fn without_abort_lines(text: &str) -> String {
  without_lines_matching(text, is_abort_line)
}

pub fn without_send_lines(text: &str) -> String {
  without_lines_matching(text, is_send_line)
}

pub fn strip_abort_marker(path: &Path, text: &str) -> Result<(), String> {
  let (body, live) = strip_live_line(text);
  let out = reassemble(&without_abort_lines(&body), "", live.as_deref());
  write_file(path, out)
}


#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
  System,
  You,
  Asst,
  ToolOutput,
}

fn build_history(tokens: &[Token<'_>], stop_above: usize, initial_mode: Mode) -> Vec<ChatMessage> {
  let mut history = Vec::new();
  let mut mode = initial_mode;
  let mut user_buf = String::new();
  let mut asst_text = String::new();
  let mut asst_reasoning = String::new();
  let mut in_thinking = false;
  let mut tool_calls: Vec<ToolCall> = Vec::new();
  let mut tool_result: Option<(String, String, String)> = None;

  fn emit_tool(
    history: &mut Vec<ChatMessage>,
    asst_text: &mut String,
    asst_reasoning: &mut String,
    tool_calls: &mut Vec<ToolCall>,
    tr: &mut Option<(String, String, String)>,
  ) {
    let (name, args, output) = tr.take().expect("tool block open");
    let id = tool_call_id(&name, &args);
    let mut calls = std::mem::take(tool_calls);
    calls.push(ToolCall::new(id.clone(), name.clone(), args));
    history.push(ChatMessage {
      role: Role::Assistant,
      content: (!asst_text.trim().is_empty()).then(|| std::mem::take(asst_text)),
      reasoning_content: (!asst_reasoning.trim().is_empty())
        .then(|| std::mem::take(asst_reasoning)),
      tool_calls: Some(calls),
      tool_call_id: None,
      name: None,
    });
    history.push(ChatMessage::tool(id, name, output));
  }

  fn emit_assistant(
    history: &mut Vec<ChatMessage>,
    asst_text: &mut String,
    asst_reasoning: &mut String,
    tool_calls: &mut Vec<ToolCall>,
  ) {
    let text = std::mem::take(asst_text);
    let reasoning = std::mem::take(asst_reasoning);
    let calls = std::mem::take(tool_calls);
    if text.trim().is_empty() && reasoning.trim().is_empty() && calls.is_empty() {
      return;
    }
    history.push(ChatMessage {
      role: Role::Assistant,
      content: (!text.trim().is_empty()).then_some(text),
      reasoning_content: (!reasoning.trim().is_empty()).then_some(reasoning),
      tool_calls: (!calls.is_empty()).then_some(calls),
      tool_call_id: None,
      name: None,
    });
  }

  for tok in tokens.iter().take_while(|t| t.line < stop_above) {
    let is_you = tok.kind == TokKind::Heading && tok.text.trim() == "### You";
    let is_asst = tok.kind == TokKind::Heading && tok.text.trim() == "### Assistant";

    match tok.kind {
      k if k.is_daemon_owned() => continue,
      TokKind::Heading if is_you && tool_result.is_none() => {
        if mode != Mode::System
          && (!asst_text.is_empty() || !asst_reasoning.is_empty() || !tool_calls.is_empty())
        {
          emit_assistant(&mut history, &mut asst_text, &mut asst_reasoning, &mut tool_calls);
        }
        user_buf.clear();
        mode = Mode::You;
      }
      TokKind::Heading if is_asst && tool_result.is_none() => {
        if mode == Mode::You {
          if !user_buf.trim().is_empty() {
            history.push(ChatMessage::user(user_buf.trim()));
          }
          user_buf.clear();
        } else if mode != Mode::System {
          emit_assistant(&mut history, &mut asst_text, &mut asst_reasoning, &mut tool_calls);
        }
        mode = Mode::Asst;
      }
      TokKind::ThinkingOpen if mode == Mode::Asst && tool_result.is_none() => {
        in_thinking = true;
      }
      TokKind::ThinkingEnd if mode == Mode::Asst && tool_result.is_none() => {
        in_thinking = false;
      }
      TokKind::Tool if tool_result.is_none() => {
        let (name, args) = tok.name_args();
        tool_result = Some((name.to_string(), args.to_string(), String::new()));
        mode = Mode::ToolOutput;
      }
      TokKind::Result if tool_result
        .as_ref()
        .is_some_and(|(open_name, _, _)| tok.name_args().0 == open_name) =>
      {
        emit_tool(&mut history, &mut asst_text, &mut asst_reasoning, &mut tool_calls, &mut tool_result);
        mode = Mode::Asst;
      }
      _ if tool_result.is_some() => {
        if let Some((_, _, out)) = tool_result.as_mut() {
          out.push_str(tok.text);
          out.push('\n');
        }
      }
      _ => match mode {
        Mode::System => {}
        Mode::You => {
          user_buf.push_str(tok.text);
          user_buf.push('\n');
        }
        Mode::Asst => {
          if in_thinking {
            asst_reasoning.push_str(tok.text);
            asst_reasoning.push('\n');
          } else {
            asst_text.push_str(tok.text);
            asst_text.push('\n');
          }
        }
        Mode::ToolOutput => unreachable!("tool block line handled above"),
      },
    }
  }

  if mode == Mode::You && !user_buf.trim().is_empty() {
    history.push(ChatMessage::user(user_buf.trim()));
  }
  if tool_result.is_some() {
    emit_tool(&mut history, &mut asst_text, &mut asst_reasoning, &mut tool_calls, &mut tool_result);
  } else {
    emit_assistant(&mut history, &mut asst_text, &mut asst_reasoning, &mut tool_calls);
  }
  history
}

pub fn tail_sections(tail: &str) -> Vec<ChatMessage> {
  build_history(&lex(tail), usize::MAX, Mode::System)
}

pub fn assistant_section_messages(raw: &str) -> Vec<ChatMessage> {
  build_history(&lex(raw), usize::MAX, Mode::Asst)
}


pub fn parse_file(text: &str) -> ParsedFile {
  let toks = lex(text);
  let n_lines = text.lines().count();

  let header_close_line = toks
    .iter()
    .find(|t| t.kind == TokKind::FenceOpen && t.payload == "toml")
    .and_then(|open| {
      toks
        .iter()
        .skip(open.line + 1)
        .find(|t| t.kind == TokKind::FenceClose)
        .map(|c| c.line)
    })
    .map(|close_line| close_line + 1);

  let mut you_lines: Vec<usize> = Vec::new();
  let mut asst_lines: Vec<usize> = Vec::new();
  for tok in &toks {
    if tok.kind == TokKind::Heading && tok.text.trim() == "### You" {
      you_lines.push(tok.line);
    }
    if tok.kind == TokKind::Heading && tok.text.trim() == "### Assistant" {
      asst_lines.push(tok.line);
    }
  }
  let first_section = you_lines
    .first()
    .copied()
    .or_else(|| asst_lines.first().copied())
    .unwrap_or(n_lines);

  let mut history: Vec<ChatMessage> = Vec::new();
  let system_raw_start = header_close_line.unwrap_or(0).min(first_section);
  if system_raw_start < first_section {
    let mut system_prompt = String::new();
    for tok in toks.iter().filter(|t| t.line >= system_raw_start && t.line < first_section) {
      if matches!(tok.kind, TokKind::FenceOpen | TokKind::FenceClose | TokKind::Heading) {
        continue;
      }
      if tok.text.trim().is_empty() {
        continue;
      }
      system_prompt.push_str(tok.text);
      system_prompt.push('\n');
    }
    let system_prompt = system_prompt.trim().to_string();
    if !system_prompt.is_empty() {
      history.push(ChatMessage::system(&system_prompt));
    }
  }

  let last_done_line = toks
    .iter()
    .rfind(|t| t.kind.is_turn_end())
    .map(|t| t.line);
  let status_line = toks.iter().rfind(|t| t.kind == TokKind::Live).map(|t| t.line);
  let history_end = match (last_done_line, status_line) {
    (Some(d), _) => d + 1,
    (None, Some(s)) => s,
    (None, None) => n_lines,
  };
  history.extend(build_history(&toks, history_end, Mode::System));

  let dangling = asst_lines.last().and_then(|&last_asst| {
    let after_ix = toks
      .iter()
      .find(|t| t.line == last_asst)
      .map(|t| t.offset)
      .unwrap_or(0);
    let has_done_after = toks.iter().any(|t| t.kind.is_turn_end() && t.line > last_asst);
    let has_next_you = you_lines.last().is_some_and(|&y| y > last_asst);
    if has_done_after || has_next_you {
      return None;
    }
    let section_end = toks
      .iter()
      .find(|t| t.line > last_asst && t.kind == TokKind::Live)
      .map(|t| t.offset)
      .unwrap_or(text.len());
    let section = &text[after_ix..section_end];
    let tools = toks
      .iter()
      .filter(|t| t.kind == TokKind::Tool && t.line > last_asst)
      .count();
    let results = toks
      .iter()
      .filter(|t| t.kind == TokKind::Result && t.line > last_asst)
      .count();
    if tools > results || (tools == 0 && !section.trim().is_empty()) {
      let heading = toks.iter().find(|t| t.line == last_asst)?;
      Some(DanglingTail {
        resume_offset: heading.line_end(),
        partial_text: text[after_ix + heading.text.len()..section_end].to_string(),
      })
    } else {
      None
    }
  });

  let tail = match last_done_line {
    Some(d) => {
      let start = toks
        .iter()
        .find(|t| t.line == d)
        .map(|t| t.offset + t.text.len())
        .unwrap_or(text.len());
      &text[start..]
    }
    None => text,
  };
  let tail = match lex(tail).iter().last() {
    Some(t) if t.kind == TokKind::Live => &tail[..t.offset.saturating_sub(1)],
    _ => tail,
  }
  .to_string();

  let pending_questions = lex(&tail)
    .iter()
    .filter(|t| t.kind == TokKind::QuestionHeader)
    .filter_map(|h| crate::questions::parse_block(&tail[h.offset..]))
    .collect();

  ParsedFile {
    config: header_config(text),
    history,
    tail,
    pending_questions,
    dangling,
  }
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod transcript_tests;
