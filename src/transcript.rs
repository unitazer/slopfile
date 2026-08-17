//! Transcript parsing: the session `.md` file is the entire UI.
//!
//! Three regions:
//! - HEADER: the FIRST ```` ```toml ```` fenced block in the file (session
//!   settings; stripped from LLM context).
//! - MIDDLE: system prompt (first text after header) + `### You` /
//!   `### Assistant` history with thinking + tool blocks and `@done` anchors.
//! - TAIL: everything below the last `@done` — user drafts and QUESTIONS
//!   blocks; the daemon-owned status line (last line) is excluded.
//!
//! All parse paths are pure: they never mutate the file.

use crate::questions::QuestionsBlock;
use crate::types::{ChatMessage, Role, SessionConfig, ToolCall};

pub const DONE_MARKER: &str = "<!-- @done -->";
pub const THINK_START: &str = "<!-- thinking -->";
pub const THINK_END: &str = "<!-- /thinking -->";
pub const STATUS_PREFIX: &str = "slopfile:live";
pub const HEADER_FENCE: &str = "```toml";

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
    pub system_prompt: String,
    pub history: Vec<ChatMessage>,
    /// Everything below the last `@done` marker (user drafts, QUESTIONS
    /// blocks). Excludes the daemon status line.
    pub tail: String,
    pub pending_questions: Vec<QuestionsBlock>,
    pub dangling: Option<DanglingTail>,
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Byte offset of line `line_idx` (0-based) in `text`.
fn line_offset(text: &str, line_idx: usize) -> usize {
    text.lines()
        .take(line_idx)
        .map(|l| l.len() + 1)
        .sum::<usize>()
}

fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Deterministic tool-call id from name+arguments (contract: first 16 hex).
fn tool_call_id(name: &str, arguments: &str) -> String {
    format!("{:016x}", fnv1a64(&format!("{name}{arguments}")))
}

/// Remove the daemon status line (last line starting with
/// `<!-- slopfile:live`).
fn strip_status_line(text: &str) -> &str {
    let trimmed_end = text.trim_end();
    match trimmed_end.rfind('\n') {
        Some(ix) => {
            let last = &trimmed_end[ix + 1..];
            if last.trim_start().starts_with(&format!("<!-- {STATUS_PREFIX}")) {
                &text[..text.len() - (trimmed_end.len() - ix - 1)]
            } else {
                text
            }
        }
        None => {
            if trimmed_end
                .trim_start()
                .starts_with(&format!("<!-- {STATUS_PREFIX}"))
            {
                ""
            } else {
                text
            }
        }
    }
}

/// Parse the header config: the FIRST ```` ```toml ```` block only.
pub fn header_config(text: &str) -> SessionConfig {
    let mut lines = text.lines();
    let Some(start_ix) = lines.position(|l| l.trim() == HEADER_FENCE) else {
        return SessionConfig::default();
    };
    let _ = start_ix;
    // Collect until the next ``` line (closing fence).
    let mut body = Vec::new();
    for line in text.lines().skip(start_ix + 1) {
        if line.trim() == "```" {
            break;
        }
        body.push(line);
    }
    let body = body.join("\n");
    if body.trim().is_empty() {
        return SessionConfig::default();
    }
    toml::from_str(&body).unwrap_or_default()
}

/// Find the byte offset of the LAST `@done` marker line. None if absent.
pub fn find_done_marker(text: &str) -> Option<usize> {
    let mut last: Option<usize> = None;
    for (i, line) in text.lines().enumerate() {
        if line.trim() == DONE_MARKER {
            last = Some(line_offset(text, i));
        }
    }
    last
}

pub fn is_dangling(text: &str) -> bool {
    parse_file(text).dangling.is_some()
}

// ── Main parse ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    System,
    You,
    Asst,
    ToolOutput,
}

/// Parse just the conversation sections of a text (no system-prompt glue):
/// the section scan step shared by `parse_file` (history up to the last
/// `@done`) and `tail_sections` (drafts / in-flight turns below `@done`).
fn scan_lines(
    lines: &[&str],
    history: &mut Vec<ChatMessage>,
    mode: &mut Mode,
    user_buf: &mut String,
    asst_text: &mut String,
    asst_reasoning: &mut String,
    in_thinking: &mut bool,
    tool_calls: &mut Vec<ToolCall>,
    tool_result: &mut Option<(String, String, String, bool)>,
) {
    // Finalize the current tool block (assistant + tool messages).
    fn emit_tool(
        history: &mut Vec<ChatMessage>,
        asst_text: &mut String,
        asst_reasoning: &mut String,
        tool_calls: &mut Vec<ToolCall>,
        tr: &mut Option<(String, String, String, bool)>,
    ) {
        let (name, args, output, _ok) = tr.take().expect("tool block open");
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

    for raw in lines {
        let line = raw.trim();

        // Section transitions (only outside a tool block).
        if tool_result.is_none() {
            match line {
                "### You" => {
                    // Close current assistant (if any).
                    if *mode != Mode::System
                        && (!asst_text.is_empty() || !asst_reasoning.is_empty() || !tool_calls.is_empty())
                    {
                        emit_assistant(history, asst_text, asst_reasoning, tool_calls);
                    }
                    user_buf.clear();
                    *mode = Mode::You;
                    continue;
                }
                "### Assistant" => {
                    if *mode == Mode::You {
                        if !user_buf.trim().is_empty() {
                            history.push(ChatMessage::user(user_buf.trim()));
                        }
                        user_buf.clear();
                    } else if *mode != Mode::System {
                        emit_assistant(history, asst_text, asst_reasoning, tool_calls);
                    }
                    *mode = Mode::Asst;
                    continue;
                }
                _ => {}
            }
        } else {
            // Inside a tool block: wait for the closing result marker.
            if line.starts_with("<!-- result:") {
                let name = line
                    .trim_start_matches("<!-- result:")
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
                emit_tool(history, asst_text, asst_reasoning, tool_calls, tool_result);
                let _ = name;
                *mode = Mode::Asst;
                continue;
            }
        }

        match mode {
            Mode::System => {
                // Extra stray content before first section: ignore.
            }
            Mode::You => {
                user_buf.push_str(raw);
                user_buf.push('\n');
            }
            Mode::Asst => {
                // Thinking markers toggle accumulation into reasoning.
                if line == THINK_START {
                    *in_thinking = true;
                    continue;
                }
                if line == THINK_END {
                    *in_thinking = false;
                    continue;
                }
                // Tool block opener: `<!-- tool:NAME ARGS_JSON -->`
                if let Some(rest) = line.strip_prefix("<!-- tool:") {
                    let Some(end_ix) = rest.find(" -->") else {
                        continue;
                    };
                    let payload = &rest[..end_ix];
                    let (name, args) = payload.split_once(' ').unwrap_or((payload, "{}"));
                    *tool_result = Some((name.to_string(), args.to_string(), String::new(), true));
                    *mode = Mode::ToolOutput;
                    continue;
                }
                // Pre-tool text / reasoning.
                if *in_thinking {
                    asst_reasoning.push_str(raw);
                    asst_reasoning.push('\n');
                } else {
                    asst_text.push_str(raw);
                    asst_text.push('\n');
                }
            }
            Mode::ToolOutput => {
                if let Some((_, _, out, _)) = tool_result.as_mut() {
                    out.push_str(raw);
                    out.push('\n');
                }
            }
        }
    }

    // Flush trailing state.
    if *mode == Mode::You && !user_buf.trim().is_empty() {
        history.push(ChatMessage::user(user_buf.trim()));
    }
    if tool_result.is_some() {
        // Unterminated tool block (resume point): emit as assistant tool call
        // with the partial output as the result.
        emit_tool(history, asst_text, asst_reasoning, tool_calls, tool_result);
    } else {
        emit_assistant(history, asst_text, asst_reasoning, tool_calls);
    }
}

/// Parse the below-`@done` region into conversation messages (drafts,
/// in-flight turns, tool blocks). The daemon appends these to the history
/// when building the next request so mid-turn state survives re-parses.
pub fn tail_sections(tail: &str) -> Vec<ChatMessage> {
    let lines: Vec<&str> = tail.lines().collect();
    let mut history = Vec::new();
    let mut mode = Mode::System;
    let mut user_buf = String::new();
    let mut asst_text = String::new();
    let mut asst_reasoning = String::new();
    let mut in_thinking = false;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut tool_result: Option<(String, String, String, bool)> = None;
    scan_lines(
        &lines, &mut history, &mut mode, &mut user_buf, &mut asst_text, &mut asst_reasoning,
        &mut in_thinking, &mut tool_calls, &mut tool_result,
    );
    history
}

pub fn parse_file(text: &str) -> ParsedFile {
    let config = header_config(text);
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();

    // ── Region boundaries ──
    // Header: first ```toml ... ``` block.
    let mut header_end_line: usize = 0;
    let mut i = 0;
    'header: while i < n {
        if lines[i].trim() == HEADER_FENCE {
            let mut j = i + 1;
            while j < n && lines[j].trim() != "```" {
                j += 1;
            }
            header_end_line = j + 1;
            break 'header;
        }
        i += 1;
    }

    // Section starts.
    let mut you_lines: Vec<usize> = Vec::new();
    let mut asst_lines: Vec<usize> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        match line.trim() {
            "### You" => you_lines.push(idx),
            "### Assistant" => asst_lines.push(idx),
            _ => {}
        }
    }
    let first_section = you_lines
        .first()
        .copied()
        .or_else(|| asst_lines.first().copied())
        .unwrap_or(n);

    // ── System prompt ──
    let mut history: Vec<ChatMessage> = Vec::new();
    let system_raw = if header_end_line > 0 && header_end_line < first_section {
        lines[header_end_line..first_section].join("\n")
    } else if header_end_line == 0 && first_section > 0 {
        lines[..first_section].join("\n")
    } else {
        String::new()
    };
    let system_prompt = system_raw
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if !system_prompt.is_empty() {
        history.push(ChatMessage::system(&system_prompt));
    }

    // ── History up to the last @done ──
    let last_done_line: Option<usize> = {
        let mut last = None;
        for (idx, line) in lines.iter().enumerate() {
            if line.trim() == DONE_MARKER {
                last = Some(idx);
            }
        }
        last
    };
    let history_end = last_done_line.map_or(n, |d| d + 1);

    let mut mode = Mode::System;
    let mut user_buf = String::new();
    let mut asst_text = String::new();
    let mut asst_reasoning = String::new();
    let mut in_thinking = false;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    // (name, args_json, output_text, ok)
    let mut tool_result: Option<(String, String, String, bool)> = None;

    scan_lines(&lines[..history_end], &mut history, &mut mode, &mut user_buf, &mut asst_text, &mut asst_reasoning, &mut in_thinking, &mut tool_calls, &mut tool_result);

    // ── Dangling detection ──
    //
    // An assistant section is "dangling" (interrupted output) only when it
    // has no `@done` below it, no `### You` after it, and its tool blocks
    // are not all closed (`<!-- result:` for every `<!-- tool:`). A closed
    // tool round is complete history — the daemon continues the turn without
    // cutting it away.
    let dangling = if let Some(&last_asst) = asst_lines.last() {
        let after_ix = line_offset(text, last_asst);
        let after = &text[after_ix..];
        let has_done_after = after.contains(DONE_MARKER);
        let has_next_you = you_lines.last().is_some_and(|&y| y > last_asst);
        if !has_done_after && !has_next_you {
            let header_off = after.find("### Assistant").unwrap_or(0) + "### Assistant".len();
            let section = &after[header_off..];
            let tools = section.matches("<!-- tool:").count();
            let results = section.matches("<!-- result:").count();
            let has_content = !section.trim().is_empty();
            if tools > results || (tools == 0 && has_content) {
                Some(DanglingTail {
                    resume_offset: after_ix + "### Assistant\n".len(),
                    partial_text: after.strip_prefix("### Assistant").unwrap_or(after).to_string(),
                })
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // ── Tail ──
    let tail_raw = match last_done_line {
        Some(d) => {
            let start = line_offset(text, d) + DONE_MARKER.len();
            &text[start..]
        }
        None => text,
    };
    let tail = strip_status_line(tail_raw).to_string();

    let pending_questions = parse_questions_once(&tail);

    let system_prompt = history
        .first()
        .and_then(|m| {
            if m.role == Role::System {
                m.content.clone()
            } else {
                None
            }
        })
        .unwrap_or_default();

    ParsedFile {
        config,
        system_prompt,
        history,
        tail,
        pending_questions,
        dangling,
    }
}

/// Scan the tail for a QUESTIONS block (single nearest one).
fn parse_questions_once(tail: &str) -> Vec<QuestionsBlock> {
    // The tail may contain at most one QUESTIONS block in practice; support
    // a list defensively by iterating (parse_block returns the first hit).
    let mut out = Vec::new();
    let mut rest = tail;
    while let Some(block) = crate::questions::parse_block(rest) {
        out.push(block);
        let start = rest.find(crate::questions::BLOCK_HEADER).unwrap_or(rest.len());
        let after = start + crate::questions::BLOCK_HEADER.len();
        rest = &rest[after..];
        // Avoid infinite loop: strip one block then re-scan.
        if let Some(next) = rest.find(crate::questions::BLOCK_HEADER) {
            rest = &rest[next..];
        } else {
            break;
        }
    }
    out
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
        assert!(pf.system_prompt.contains("You are a coding agent."));
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
        assert!(!is_dangling(&txt));

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
        assert!(pf.system_prompt.contains("Direct system text"));
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
}