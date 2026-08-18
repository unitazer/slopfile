//! Transcript parsing: the session `.md` file is the entire UI, and its
//! grammar is defined once in `lex.rs`. The parser here consumes the lexer's
//! token stream — no text is re-scanned with string searches.
//!
//! Three regions:
//! - HEADER: the FIRST ```` ```toml ```` fenced block in the file (session
//!   settings; stripped from LLM context).
//! - MIDDLE: system prompt (first text after header) + `### You` /
//!   `### Assistant` history with thinking + tool blocks and `@done` anchors.
//! - TAIL: everything below the last `@done` — user drafts and QUESTIONS
//!   blocks; the daemon-owned status line is excluded.
//!
//! All parse paths are pure: they never mutate the file.

use crate::lex::{lex, lex_line, Token, TokKind};
use crate::questions::QuestionsBlock;
use crate::types::{ChatMessage, Role, SessionConfig, ToolCall};

pub use crate::lex::{ABORT_MARKER, DONE_MARKER};

/// True when `text` contains an abort request (an `Abort` token: the marker
/// on its own line, or glued after the status line). Prose that merely
/// mentions the marker is `Text` and can never match.
pub fn abort_marker_present(text: &str) -> bool {
  lex(text).iter().any(|t| t.kind == TokKind::Abort)
}

/// True for a single line that IS the abort marker.
pub fn is_abort_line(line: &str) -> bool {
  lex_line(line) == TokKind::Abort
}

/// Remove the daemon status line: find the `Live` token, drop exactly that
/// line. Returns the body and the removed status line (if any). The single
/// source of truth for live-line handling — the daemon, the parser and the
/// QoL commands all go through here.
pub fn strip_live_line(text: &str) -> (String, Option<String>) {
  let toks = lex(text);
  let Some(live) = toks.iter().find(|t| t.kind == TokKind::Live) else {
    return (text.to_string(), None);
  };
  let mut body = String::new();
  for (i, tok) in toks.iter().enumerate() {
    if i == live.line {
      continue;
    }
    body.push_str(tok.text);
    body.push('\n');
  }
  while body.ends_with('\n') && !body.is_empty() {
    body.pop();
  }
  body.push('\n');
  (body, Some(live.text.to_string()))
}

// ── Parsed output ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DanglingTail {
  /// Byte offset where resumed output should start (right after the
  /// incomplete assistant's last written text).
  pub resume_offset: usize,
  /// The partial text of the incomplete assistant turn.
  pub partial_text: String,
}

#[derive(Debug, Clone)]
pub struct ParsedFile {
  pub config: SessionConfig,
  pub history: Vec<ChatMessage>,
  /// Everything below the last `@done` marker (user drafts, QUESTIONS
  /// blocks). Excludes the daemon status line.
  pub tail: String,
  pub pending_questions: Vec<QuestionsBlock>,
  pub dangling: Option<DanglingTail>,
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Deterministic tool-call id from name+arguments (contract: first 16 hex).
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

/// Byte offset just past the complete raw line of token `t`.
fn line_end(t: &Token) -> usize {
  t.offset + t.text.len() + 1
}

/// Parse the header config: the FIRST ```` ```toml ```` block only.
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

/// Byte offset of the LAST `@done` marker line. None if absent.
pub fn find_done_marker(text: &str) -> Option<usize> {
  lex(text)
    .iter()
    .filter(|t| t.kind == TokKind::Done)
    .last()
    .map(|t| t.offset)
}

// ── Message building ────────────────────────────────────────────────────

/// Conversation-state machine over the token stream.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
  System,
  You,
  Asst,
  ToolOutput,
}

/// Accumulate conversation messages from the tokens strictly above
/// `stop_above` (token.line < stop_above). Mirrors the transcript grammar:
/// section headings switch mode, thinking/tool/result markers are
/// structural, everything else (including `@done` and code fences) is
/// message text. Daemon-owned lines are never content.
fn build_history(tokens: &[Token<'_>], stop_above: usize) -> Vec<ChatMessage> {
  let mut history = Vec::new();
  let mut mode = Mode::System;
  let mut user_buf = String::new();
  let mut asst_text = String::new();
  let mut asst_reasoning = String::new();
  let mut in_thinking = false;
  let mut tool_calls: Vec<ToolCall> = Vec::new();
  // (name, args_json, output_text)
  let mut tool_result: Option<(String, String, String)> = None;

  // Finalize the current tool block (assistant + tool messages).
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
    // Assistant message carrying the call (with whatever text/reasoning
    // preceded it inside this section).
    history.push(ChatMessage {
      role: Role::Assistant,
      content: (!asst_text.trim().is_empty()).then(|| std::mem::take(asst_text)),
      parts: None,
      reasoning_content: (!asst_reasoning.trim().is_empty())
        .then(|| std::mem::take(asst_reasoning)),
      tool_calls: Some(calls),
      tool_call_id: None,
      name: None,
    });
    history.push(ChatMessage::tool(id, name, output));
  }

  // Finalize the current assistant plain message (or with accumulated calls).
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
      parts: None,
      reasoning_content: (!reasoning.trim().is_empty()).then_some(reasoning),
      tool_calls: (!calls.is_empty()).then_some(calls),
      tool_call_id: None,
      name: None,
    });
  }

  for tok in tokens.iter().take_while(|t| t.line < stop_above) {
    let is_you = tok.kind == TokKind::Heading && tok.payload == "You";
    let is_asst = tok.kind == TokKind::Heading && tok.payload == "Assistant";

    match tok.kind {
      // Daemon-owned control lines never reach messages.
      k if k.is_daemon_owned() => continue,
      // Section transitions (only outside a tool block).
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
      // Thinking markers toggle accumulation.
      TokKind::ThinkingOpen => in_thinking = true,
      TokKind::ThinkingEnd => in_thinking = false,
      // Tool round: open on `<!-- tool:`, close on `<!-- result:`. While a
      // block is open all lines accumulate as the tool's output.
      TokKind::Tool if tool_result.is_none() => {
        let (name, args) = tok.name_args();
        tool_result = Some((name.to_string(), args.to_string(), String::new()));
        mode = Mode::ToolOutput;
      }
      TokKind::Result if tool_result.is_some() => {
        emit_tool(&mut history, &mut asst_text, &mut asst_reasoning, &mut tool_calls, &mut tool_result);
        mode = Mode::Asst;
      }
      _ if tool_result.is_some() => {
        // Inside an open tool block: raw output text.
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

  // Flush trailing state.
  if mode == Mode::You && !user_buf.trim().is_empty() {
    history.push(ChatMessage::user(user_buf.trim()));
  }
  if tool_result.is_some() {
    // Unterminated tool block (resume point): emit as assistant tool call
    // with the partial output as the result.
    emit_tool(&mut history, &mut asst_text, &mut asst_reasoning, &mut tool_calls, &mut tool_result);
  } else {
    emit_assistant(&mut history, &mut asst_text, &mut asst_reasoning, &mut tool_calls);
  }
  history
}

/// Parse the below-`@done` region into conversation messages (drafts,
/// in-flight turns, tool blocks). The daemon appends these to the history
/// when building the next request so mid-turn state survives re-parses.
pub fn tail_sections(tail: &str) -> Vec<ChatMessage> {
  build_history(&lex(tail), usize::MAX)
}

// ── Main parse ──────────────────────────────────────────────────────────

pub fn parse_file(text: &str) -> ParsedFile {
  let toks = lex(text);
  let n_lines = text.lines().count();

  // ── Region boundaries (token positions) ──
  // Header: first ```toml ... ``` block.
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

  // Section starts.
  let mut you_lines: Vec<usize> = Vec::new();
  let mut asst_lines: Vec<usize> = Vec::new();
  for tok in &toks {
    if tok.kind == TokKind::Heading && tok.payload == "You" {
      you_lines.push(tok.line);
    }
    if tok.kind == TokKind::Heading && tok.payload == "Assistant" {
      asst_lines.push(tok.line);
    }
  }
  let first_section = you_lines
    .first()
    .copied()
    .or_else(|| asst_lines.first().copied())
    .unwrap_or(n_lines);

  // ── System prompt ──
  let mut history: Vec<ChatMessage> = Vec::new();
  let system_raw_start = header_close_line.unwrap_or(0).min(first_section);
  if system_raw_start < first_section {
    let mut system_prompt = String::new();
    for tok in toks.iter().filter(|t| t.line >= system_raw_start && t.line < first_section) {
      // Fence lines stay out; comment- and heading-style lines are not
      // prompt text.
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

  // ── History up to the last @done (never past the status line) ──
  let last_done_line = toks
    .iter()
    .filter(|t| t.kind == TokKind::Done)
    .last()
    .map(|t| t.line);
  let status_line = toks.iter().filter(|t| t.kind == TokKind::Live).last().map(|t| t.line);
  let history_end = match (last_done_line, status_line) {
    (Some(d), Some(s)) if s > d => s,
    (Some(d), _) => d + 1,
    (None, Some(s)) => s,
    (None, None) => n_lines,
  };
  history.extend(build_history(&toks, history_end));

  // ── Dangling detection ──
  //
  // An assistant section is "dangling" (interrupted output) only when it
  // has no `@done` below it, no `### You` after it, and its tool blocks
  // are not all closed (`<!-- result:` for every `<!-- tool:`). A closed
  // tool round is complete history — the daemon continues the turn without
  // cutting it away.
  let dangling = asst_lines.last().and_then(|&last_asst| {
    let after_ix = toks
      .iter()
      .find(|t| t.line == last_asst)
      .map(|t| t.offset)
      .unwrap_or(0);
    let has_done_after = toks.iter().any(|t| t.kind == TokKind::Done && t.line > last_asst);
    let has_next_you = you_lines.last().is_some_and(|&y| y > last_asst);
    if has_done_after || has_next_you {
      return None;
    }
    let section = &text[after_ix..];
    let tools = toks
      .iter()
      .filter(|t| t.kind == TokKind::Tool && t.line > last_asst)
      .count();
    let results = toks
      .iter()
      .filter(|t| t.kind == TokKind::Result && t.line > last_asst)
      .count();
    if tools > results || (tools == 0 && !section.trim().is_empty()) {
      // Heading token (the raw `### Assistant` line) sits at after_ix.
      let heading = toks.iter().find(|t| t.line == last_asst)?;
      Some(DanglingTail {
        resume_offset: line_end(heading),
        partial_text: text[after_ix + heading.text.len()..].to_string(),
      })
    } else {
      None
    }
  });

  // ── Tail ──
  let tail = match last_done_line {
    Some(d) => {
      let start = toks
        .iter()
        .find(|t| t.line == d)
        .map(|t| t.offset + DONE_MARKER.len())
        .unwrap_or(text.len());
      &text[start..]
    }
    None => text,
  };
  // A trailing status line is daemon-owned: never part of the tail.
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
mod tests {
  use super::*;
  use crate::types::Role;

  fn sample() -> String {
    r#"```toml
model = "qwen3.8-27b"
thinking = "auto"
tools = ["read","bash"]
```
# Session
You are a coding agent.

### You
Hello, agent.

### Assistant
Hi there.

### You
Show me the files.

### Assistant
Let me check.
<!-- thinking -->
I will ls first.
<!-- /thinking -->
<!-- tool:bash {"cmd":"ls"} -->
src/
docs/
<!-- result:bash ok=42ms -->
Here is the listing.
<!-- @done -->
"#
    .to_string()
  }

  #[test]
  fn header_parsed_from_first_fence_only() {
    let cfg = header_config(&sample());
    assert_eq!(cfg.model.as_deref(), Some("qwen3.8-27b"));
    assert_eq!(
      cfg.tools.as_deref(),
      Some(["read".to_string(), "bash".to_string()].as_slice())
    );

    // A later ```toml fence in assistant text must NOT be parsed.
    let txt2 = format!(
      "{}\n### Assistant\n```toml\nmodel = \"hacked\"\n```\nThis is code.\n",
      sample()
    );
    let cfg2 = header_config(&txt2);
    assert_eq!(cfg2.model.as_deref(), Some("qwen3.8-27b"));
  }

  #[test]
  fn system_prompt_extracted() {
    let pf = parse_file(&sample());
    assert!(pf.history[0].content.as_deref().unwrap().contains("You are a coding agent."));
    assert_eq!(pf.history[0].role, Role::System);
  }

  #[test]
  fn history_round_trip_with_tools_and_thinking() {
    let pf = parse_file(&sample());
    let roles: Vec<Role> = pf.history.iter().map(|m| m.role).collect();
    assert_eq!(
      roles,
      vec![
        Role::System,
        Role::User,
        Role::Assistant,
        Role::User,
        Role::Assistant,
        Role::Tool,
        Role::Assistant
      ]
    );

    let asst_with_call = pf
      .history
      .iter()
      .find(|m| m.tool_calls.is_some())
      .expect("tool call");
    assert_eq!(asst_with_call.tool_calls.as_ref().unwrap()[0].name, "bash");
    assert!(asst_with_call
      .reasoning_content
      .as_deref()
      .unwrap()
      .contains("I will ls"));

    let tool_msg = pf.history.iter().find(|m| m.role == Role::Tool).expect("tool msg");
    assert!(tool_msg.content.as_deref().unwrap().contains("src/"));
    // Tool message references the same id.
    assert_eq!(
      asst_with_call.tool_calls.as_ref().unwrap()[0].id,
      tool_msg.tool_call_id.as_deref().unwrap()
    );
  }

  #[test]
  fn dangling_detected() {
    let mut txt = sample();
    txt.push_str("### You\nNext.\n");
    assert!(parse_file(&txt).dangling.is_none());

    let mut txt2 = sample();
    txt2.push_str("### Assistant\npartial output without done\n");
    let pf = parse_file(&txt2);
    assert!(pf.dangling.is_some());
    let d = pf.dangling.as_ref().unwrap();
    assert!(d.partial_text.contains("partial output without done"));
    assert!(d.resume_offset > 0);
  }

  #[test]
  fn status_line_excluded() {
    let mut txt = sample();
    txt.push_str("\n<!-- slopfile:live {\"phase\":\"idle\"} -->\n");
    let pf = parse_file(&txt);
    assert!(!pf.tail.contains("slopfile:live"));
    assert!(!pf.tail.contains("{\"phase\":\"idle\"}"));
    // The status line must never leak into history as an assistant turn.
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:live"))));
  }

  #[test]
  fn status_line_after_closed_tool_block_is_not_history() {
    // No @done: history ends with a closed tool round, then the daemon
    // status line. The status line must NOT become a trailing Assistant.
    let txt = "\
```toml
model = \"qwen3.8-27b\"
```
# Smoke
You are a helpful test agent.

### You
Question?

### Assistant
<!-- thinking -->
Which questions?
<!-- /thinking -->
I will ask.
<!-- tool:question {\"desc\":\"x\"} -->
answers json
<!-- result:question ok=ask -->

<!-- slopfile:live {\"phase\":\"waiting_answer\",\"model\":\"qwen3.8-27b\"} -->
";
    let pf = parse_file(txt);
    assert_eq!(pf.history.last().map(|m| m.role), Some(Role::Tool));
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:live"))));
    assert!(pf.dangling.is_none());
  }

  #[test]
  fn tail_is_below_done() {
    let mut txt = sample();
    txt.push_str("NEW DRAFT HERE\n");
    let pf = parse_file(&txt);
    assert!(pf.tail.contains("NEW DRAFT HERE"));
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("NEW DRAFT"))));
  }

  #[test]
  fn done_marker_found() {
    assert!(find_done_marker(&sample()).is_some());
    assert!(parse_file(&sample()).dangling.is_none());
  }

  #[test]
  fn no_header_means_system_prompt_is_prefix() {
    let txt = "Direct system text.\n\n### You\nhi\n";
    let pf = parse_file(&txt);
    assert_eq!(pf.history[0].role, Role::System);
    assert!(pf.history[0]
      .content
      .as_deref()
      .unwrap()
      .contains("Direct system text"));
  }

  #[test]
  fn unterminated_tool_block_is_resumed() {
    let mut txt = sample();
    txt.push_str("### Assistant\n<!-- tool:bash {\"cmd\":\"sleep\"} -->\n");
    let pf = parse_file(&txt);
    // The tool call is emitted even without a result marker.
    assert!(pf.history.iter().any(|m| m.tool_calls.is_some()));
  }

  #[test]
  fn questions_in_tail_are_detected() {
    let mut txt = sample();
    txt.push_str("\n### You\nwhat do you think?\n");
    txt.push_str(&crate::questions::render_block(&[crate::questions::Question {
      id: 1,
      question: "red or blue".to_string(),
      answers: vec!["red".into(), "blue".into()],
      default: None,
    }]));
    let pf = parse_file(&txt);
    assert_eq!(pf.pending_questions.len(), 1);
    assert_eq!(pf.pending_questions[0].questions[0].question, "red or blue");
    // The block stays in the tail text too.
    assert!(pf.tail.contains("❓ QUESTIONS"));
  }

  #[test]
  fn markers_inside_closed_fence_are_not_structural() {
    let txt = "\
### Assistant
```md
### You
<!-- @done -->
```
### Assistant
real reply
";
    let pf = parse_file(&txt);
    // The fenced-in `### You` must not create a user turn; the fence content
    // stays assistant text.
    let asst_msgs = pf
      .history
      .iter()
      .filter(|m| m.role == Role::Assistant)
      .count();
    assert_eq!(asst_msgs, 2);
    let last = pf.history.last().unwrap();
    assert_eq!(last.content.as_deref().unwrap().trim(), "real reply");
  }

  #[test]
  fn abort_marker_is_line_based() {
    assert!(abort_marker_present("hi\n<!-- slopfile:abort -->\n"));
    assert!(abort_marker_present(
      "<!-- slopfile:live {\"x\":1} --><!-- slopfile:abort -->\n"
    ));
    assert!(!abort_marker_present(
      "47: - Abort: `<!-- slopfile:abort -->` marker line\n"
    ));
    assert!(is_abort_line("<!-- slopfile:abort -->"));
    assert!(!is_abort_line("  prose <!-- slopfile:abort --> mention"));
  }

  #[test]
  fn strip_live_line_removes_only_the_live_line() {
    let src = "a\nb\n<!-- slopfile:live {\"phase\":\"idle\"} -->\n";
    let (body, live) = strip_live_line(src);
    assert_eq!(body, "a\nb\n");
    assert!(live.is_some());
    assert_eq!(strip_live_line("plain\n").0, "plain\n");
  }
}