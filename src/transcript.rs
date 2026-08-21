
use crate::lex::{lex, lex_line, Token, TokKind};
use crate::questions::QuestionsBlock;
use crate::types::{ChatMessage, Role, SessionConfig, ToolCall};

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

pub fn strip_live_line(text: &str) -> (String, Option<(usize, String)>) {
  let toks = lex(text);
  let mut last = None;
  let mut body = String::new();
  for tok in toks.iter() {
    if tok.kind == TokKind::Live {
      last = Some((tok.offset, tok.text.to_string()));
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
  fn dangling_partial_replays_as_assistant_messages() {
    let partial = "<!-- thinking -->\nlet me think about this\n<!-- /thinking -->\nContinue the work.\n";
    let msgs = assistant_section_messages(partial);
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!(m.role, Role::Assistant);
    assert!(m
      .content
      .as_deref()
      .is_some_and(|c| c.contains("Continue the work")));
    assert!(m
      .reasoning_content
      .as_deref()
      .is_some_and(|r| r.contains("let me think about this")));
  }

  #[test]
  fn dangling_partial_prose_only_is_content() {
    let msgs = assistant_section_messages("still working on it\n");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::Assistant);
    assert!(msgs[0]
      .content
      .as_deref()
      .is_some_and(|c| c.starts_with("still working")));
  }

  #[test]
  fn status_line_excluded() {
    let mut txt = sample();
    txt.push_str("\n<!-- slopfile:live {\"phase\":\"idle\"} -->\n");
    let pf = parse_file(&txt);
    assert!(!pf.tail.contains("slopfile:live"));
    assert!(!pf.tail.contains("{\"phase\":\"idle\"}"));
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:live"))));
  }

  #[test]
  fn aborted_marker_bounds_the_turn_like_done() {
    let txt = r#"```toml
model = "qwen3.8-27b"
```
# Sm
Sys.

### You
Question?

### Assistant
Let me check.
<!-- thinking -->
will ls
<!-- /thinking -->
<!-- tool:bash {"cmd":"ls"} -->
src/
<!-- result:bash ok=1ms -->
partial reply cut off mid
<!-- /thinking -->
<!-- slopfile:aborted -->
<!-- slopfile:live {"phase":"idle","model":"qwen3.8-27b"} -->
"#;

    let pf = parse_file(txt);
    assert!(pf.dangling.is_none(), "aborted turn must not be resumable");
    assert!(pf.tail.trim().is_empty(), "no tail below aborted turn");
    assert!(find_done_marker(txt).is_some(), "aborted marker bounds history");
    assert!(txt.contains("<!-- slopfile:aborted -->"), "marker stays visible");
    let last = pf.history.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:aborted"))));
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:live"))));
  }

  #[test]
  fn status_line_after_closed_tool_block_is_not_history() {
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
    let pf = parse_file(txt);
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
    assert!(pf.tail.contains("QUESTIONS"));
  }

  #[test]
  fn markers_inside_closed_fence_are_not_structural() {
    let txt = "\
### Assistant
```md
<!-- tool:bash {\"cmd\":\"ls\"} -->
some code
```
### Assistant
real reply
";
    let pf = parse_file(txt);
    let asst_msgs = pf
      .history
      .iter()
      .filter(|m| m.role == Role::Assistant)
      .count();
    assert_eq!(asst_msgs, 2);
    assert!(!pf
      .history
      .iter()
      .any(|m| m.tool_calls.is_some()), "fenced tool marker must not open a block");
    let last = pf.history.last().unwrap();
    assert_eq!(last.content.as_deref().unwrap().trim(), "real reply");
  }

  #[test]
  fn only_level_three_headings_transition_sections() {
    let txt = "\
### You
first
## You
draft not a transition
# Assistant
more draft
#### You
still draft
### Assistant
reply
### You
second
";
    let pf = parse_file(txt);
    let roles: Vec<Role> = pf.history.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
    let users: Vec<&str> = pf
      .history
      .iter()
      .filter(|m| m.role == Role::User)
      .flat_map(|m| m.content.as_deref())
      .collect();
    assert!(users[0].contains("first"));
    assert!(users[0].contains("## You"), "lower-level headings stay in the draft");
    assert!(users[0].contains("# Assistant"), "lower-level headings stay in the draft");
    assert!(users[1].contains("second"));
  }

  #[test]
  fn thinking_markers_only_toggle_inside_assistant_prose() {
    let txt = "\
### You
<!-- thinking -->
user pasted a marker
#### /thinking more text
### Assistant
plain
### You
<!-- /thinking -->
still user text
";
    let pf = parse_file(txt);
    let asst = pf
      .history
      .iter()
      .find(|m| m.role == Role::Assistant)
      .unwrap();
    assert_eq!(asst.reasoning_content, None, "markers outside assistant prose never toggle");
    assert!(asst.content.as_deref().unwrap().contains("plain"));
    let users: Vec<&str> = pf
      .history
      .iter()
      .filter(|m| m.role == Role::User)
      .flat_map(|m| m.content.as_deref())
      .collect();
    assert!(users[0].contains("<!-- thinking -->"), "marker stays as user text");
    assert!(users[0].contains("user pasted a marker"));
    assert_eq!(users[0].find("There is"), None);
  }

  #[test]
  fn thinking_markers_inside_tool_block_are_output_text() {
    let txt = "\
### Assistant
<!-- tool:bash {\"cmd\":\"ls\"} -->
<!-- thinking -->
<!-- /thinking -->
out
<!-- result:bash ok=1ms -->
done
";
    let pf = parse_file(txt);
    let tool = pf.history.iter().find(|m| m.role == Role::Tool).unwrap();
    let c = tool.content.as_deref().unwrap();
    assert!(c.contains("<!-- thinking -->"), "marker is tool output");
    assert!(c.contains("<!-- /thinking -->"), "marker is tool output");
    assert!(c.contains("out"));
    assert_eq!(pf.history.last().unwrap().role, Role::Assistant);
  }

  #[test]
  fn mismatched_result_name_does_not_close_the_block() {
    let txt = "\
### Assistant
<!-- tool:bash {\"cmd\":\"ls\"} -->
src/
<!-- result:grep ok=1ms -->
docs/
<!-- result:bash ok=42ms -->
done
";
    let pf = parse_file(txt);
    let tool = pf.history.iter().find(|m| m.role == Role::Tool).unwrap();
    let c = tool.content.as_deref().unwrap();
    assert!(c.contains("src/"));
    assert!(c.contains("<!-- result:grep ok=1ms -->"), "mismatched result stays in output");
    assert!(c.contains("docs/"));
    assert!(!c.contains("<!-- result:bash ok=42ms -->"), "matching result is structural");
    assert_eq!(pf.history.last().unwrap().role, Role::Assistant);
    assert_eq!(pf.history.last().unwrap().content.as_deref(), Some("done\n"));
  }

  #[test]
  fn unclosed_fence_below_next_turn_is_not_retroactive() {
    let txt = "\
### Assistant
```json
{\"x\": 1}
<!-- @done -->
### You
what now?
";
    let pf = parse_file(txt);
    assert!(find_done_marker(txt).is_some(), "@done still a done marker");
    let msgs = tail_sections(&pf.tail);
    let last = msgs.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(last.content.as_deref().unwrap().contains("what now?"));
  }

  #[test]
  fn crlf_file_parses_without_panic_and_exact_history() {
    let txt = "\
### You\r
Hello ❓ friend.\r
\r
### Assistant\r
Reply ❓ ok.\r
<!-- @done -->\r
QUESTIONS\r
\r
| # | Question | Options |\r
| 1 | red or blue | red, blue |\r
ANSWER: (numbers or text, comma separated)\r
> \r
";
    let pf = parse_file(txt);
    assert_eq!(pf.history[0].role, Role::User);
    assert!(pf.history[0]
      .content
      .as_deref()
      .unwrap()
      .contains("Hello ❓ friend."));
    assert_eq!(pf.history[1].role, Role::Assistant);
    assert!(pf.history[1]
      .content
      .as_deref()
      .unwrap()
      .contains("Reply ❓ ok."));
    assert_eq!(pf.pending_questions.len(), 1);
    assert!(pf.tail.trim_start().starts_with("QUESTIONS"));
    assert!(pf.dangling.is_none());
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
    let (off, text) = live.expect("live line removed");
    assert_eq!(off, src.find("<!-- slopfile:live").unwrap());
    assert_eq!(text, "<!-- slopfile:live {\"phase\":\"idle\"} -->");
    assert_eq!(strip_live_line("plain\n").0, "plain\n");
  }

  #[test]
  fn strip_live_line_removes_all_live_lines_and_reports_last_offset() {
    let src = "a\n<!-- slopfile:live {\"phase\":\"idle\"} -->\nb\n<!-- slopfile:live {\"phase\":\"gen\"} -->\nc";
    let (body, live) = strip_live_line(src);
    let (off, text) = live.expect("last live line reported");
    assert_eq!(text, "<!-- slopfile:live {\"phase\":\"gen\"} -->");
    assert_eq!(off, src.rfind("<!-- slopfile:live {\"phase\":\"gen\"").unwrap());
    assert!(!body.contains("slopfile:live"), "all live lines removed");
    assert_eq!(body, "a\nb\nc\n");
  }
}