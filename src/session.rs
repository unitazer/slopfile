use crate::questions::Question;
pub use crate::types::SessionConfig;
use crate::types::{ChatMessage, Role, ToolCall};
use serde::{Deserialize, Serialize};

pub const BEGIN: &str = "begin";
pub const SYSTEM: &str = "system";
pub const TURN: &str = "turn";
pub const USER: &str = "user";
pub const NOTE: &str = "note";
pub const ASSISTANT: &str = "assistant";
pub const REASONING: &str = "reasoning";
pub const TEXT: &str = "text";
pub const CALL: &str = "call";
pub const RESULT: &str = "result";
pub const QUESTION: &str = "question";
pub const INPUT: &str = "input";

const TOKEN_OPEN: &str = "<!-- slopfile:";
const TOKEN_CLOSE: &str = "<!-- /slopfile -->";
const ESC_OPEN: &str = "<!--slopfile:ESC[";
const ESC_CLOSE: &str = "<!--/slopfile:ESC-->";

/// Both wrapper delimiters start with `<`, so escaping `<` alone is enough to make it impossible for
/// a body — even one that assembles a marker across two copied runs — to produce structure.
pub const ESC_TOKENS: &[&str] = &["<"];

pub const AWAITING_ANSWER: &str = "(awaiting the user's answer)";

const LEAF_REGIONS: &[&str] = &[SYSTEM, USER, NOTE, REASONING, TEXT, CALL, RESULT, QUESTION, INPUT];
const CONTAINER_REGIONS: &[&str] = &[TURN, ASSISTANT];

pub fn escape(body: &str) -> String {
  let bytes = body.as_bytes();
  let mut out = String::with_capacity(body.len());
  let mut copied = 0usize;
  let mut ix = 0usize;
  while ix < bytes.len() {
    let Some(token) = token_at(bytes, ix) else {
      ix += 1;
      continue;
    };
    out.push_str(&body[copied..ix]);
    out.push_str(ESC_OPEN);
    out.push_str(token);
    out.push_str(ESC_CLOSE);
    ix += token.len();
    copied = ix;
  }
  out.push_str(&body[copied..]);
  out
}

fn token_at(bytes: &[u8], ix: usize) -> Option<&'static str> {
  match bytes[ix] {
    b'<' => ESC_TOKENS.first().copied(),
    _ => None,
  }
}

pub fn unescape(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(ix) = rest.find(ESC_OPEN) {
    out.push_str(&rest[..ix]);
    let inner = &rest[ix + ESC_OPEN.len()..];
    match inner.find(ESC_CLOSE) {
      Some(end) => {
        out.push_str(&inner[..end]);
        rest = &inner[end + ESC_CLOSE.len()..];
      }
      None => {
        out.push_str(ESC_OPEN);
        out.push_str(inner);
        rest = "";
      }
    }
  }
  out.push_str(rest);
  out
}

fn open_marker(line: &str) -> Option<(&str, &str)> {
  let t = line.trim();
  let inner = t.strip_prefix(TOKEN_OPEN)?.strip_suffix("-->")?;
  match inner.find(char::is_whitespace) {
    Some(ix) => Some((&inner[..ix], inner[ix..].trim())),
    None => Some((inner, "")),
  }
}

fn is_close(line: &str) -> bool {
  line.trim() == TOKEN_CLOSE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoteBy {
  #[default]
  Model,
  Harness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
  pub by: NoteBy,
  pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
  #[serde(default)]
  pub id: String,
  #[serde(default)]
  pub name: String,
  #[serde(default)]
  pub arguments: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assistant {
  pub reasoning: Option<String>,
  pub text: Option<String>,
  pub calls: Vec<Call>,
}

impl Assistant {
  pub fn has_output(&self) -> bool {
    self.reasoning.is_some() || self.text.is_some() || !self.calls.is_empty()
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultHeader {
  #[serde(default)]
  pub id: String,
  #[serde(default)]
  pub name: String,
  #[serde(default = "yes")]
  pub ok: bool,
  #[serde(default)]
  pub ms: u64,
}

fn yes() -> bool {
  true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
  pub id: String,
  pub name: String,
  pub ok: bool,
  pub ms: u64,
  pub output: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingQuestion {
  #[serde(default)]
  pub call: Call,
  #[serde(default)]
  pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Round {
  pub assistant: Assistant,
  pub results: Vec<ToolOutcome>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
  pub user: Option<String>,
  pub notes: Vec<Note>,
  pub rounds: Vec<Round>,
  pub question: Option<PendingQuestion>,
}

impl Turn {
  pub fn is_empty(&self) -> bool {
    self.user.is_none() && self.notes.is_empty() && self.rounds.is_empty() && self.question.is_none()
  }

  pub fn has_output(&self) -> bool {
    self.rounds.iter().any(|r| r.assistant.has_output() || !r.results.is_empty())
  }

  /// The model still owes a reply when its last round came back with tool results it has not
  /// reacted to yet. A round that produced nothing owes nothing: retrying an empty response just
  /// burns the GPU forever. An interrupted round is caught by `Session::turn_in_progress`.
  pub fn needs_reply(&self) -> bool {
    match self.rounds.last() {
      Some(round) => !round.results.is_empty(),
      None => self.user.is_some() || !self.notes.is_empty(),
    }
  }

  /// A turn is finished once its last round answered everything it was asked to.
  pub fn is_complete(&self) -> bool {
    !self.needs_reply()
  }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
  pub preamble: String,
  pub config: SessionConfig,
  pub system: String,
  pub turns: Vec<Turn>,
  pub input: String,
  /// Region names of the innermost still-unclosed region, outermost first. That single fact is
  /// what "in progress" means: rendering leaves the last one open, and reading recovers it.
  pub open: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseReport {
  pub unknown_regions: Vec<String>,
  pub duplicate_close: bool,
  pub swallowed: Vec<String>,
  pub bad_payloads: Vec<String>,
  pub misplaced: Vec<String>,
}

impl ParseReport {
  pub fn is_clean(&self) -> bool {
    self.unknown_regions.is_empty()
      && !self.duplicate_close
      && self.swallowed.is_empty()
      && self.bad_payloads.is_empty()
      && self.misplaced.is_empty()
  }

  pub fn problems(&self) -> Vec<String> {
    let mut out = Vec::new();
    for name in &self.unknown_regions {
      out.push(format!("unknown region `{name}`"));
    }
    if self.duplicate_close {
      out.push(format!("`{TOKEN_CLOSE}` closes a region that was never opened"));
    }
    for line in &self.swallowed {
      out.push(line.clone());
    }
    for line in &self.bad_payloads {
      out.push(line.clone());
    }
    for line in &self.misplaced {
      out.push(line.clone());
    }
    out
  }
}

/// A payload that does not decode is a file problem, not something to paper over with a default:
/// a fabricated `ok: true` result would tell the model a tool succeeded when it never ran.
fn decode_payload<T: serde::de::DeserializeOwned>(
  name: &str,
  path: &[String],
  payload: &str,
  problems: &mut Vec<String>,
) -> Option<T> {
  match serde_json::from_str::<T>(payload) {
    Ok(value) => Some(value),
    Err(e) => {
      problems.push(format!(
        "`{}` in {} has a bad header: {e}",
        name,
        region_path(path)
      ));
      None
    }
  }
}

fn region_path(path: &[String]) -> String {
  if path.is_empty() {
    return "the preamble".to_string();
  }
  format!("`{}`", path.join(" > "))
}

#[derive(Debug, Clone, Default)]
struct Node {
  name: String,
  payload: String,
  lines: Vec<String>,
  children: Vec<Node>,
}

impl Node {
  fn body(&self) -> String {
    self.lines.join("\n")
  }
}

pub fn toml_fence(config: &SessionConfig) -> String {
  let mut out = String::from("```toml\n");
  if let Some(model) = &config.model {
    out.push_str(&format!("model = \"{model}\"\n"));
  }
  out.push_str("thinking = \"auto\"\n");
  if let Some(tools) = &config.tools {
    let list = tools.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(", ");
    out.push_str(&format!("tools = [{list}]\n"));
  }
  if !config.tools_excluded.is_empty() {
    let list = config
      .tools_excluded
      .iter()
      .map(|t| format!("\"{t}\""))
      .collect::<Vec<_>>()
      .join(", ");
    out.push_str(&format!("tools_excluded = [{list}]\n"));
  }
  if let Some(n) = config.max_context_tokens {
    out.push_str(&format!("max_context_tokens = {n}\n"));
  }
  if let Some(n) = config.tool_timeout_ms {
    out.push_str(&format!("tool_timeout_ms = {n}\n"));
  }
  out.push_str("```\n");
  out
}

/// Index of the first line after the ```toml fence, or 0 when there is no closed fence.
fn fence_end_line(lines: &[&str]) -> usize {
  let mut inside = false;
  for (ix, line) in lines.iter().enumerate() {
    let t = line.trim();
    if !inside {
      if t
        .strip_prefix("```")
        .is_some_and(|rest| rest.trim().eq_ignore_ascii_case("toml"))
      {
        inside = true;
      }
      continue;
    }
    if t.starts_with("```") {
      return ix + 1;
    }
  }
  0
}

/// Byte range of the ```toml fence, including its closing fence line.
pub fn fence_bounds(preamble: &str) -> Option<(usize, usize)> {
  let mut at = 0usize;
  let mut start = None;
  for line in preamble.split_inclusive('\n') {
    let t = line.trim();
    match start {
      None => {
        if t
          .strip_prefix("```")
          .is_some_and(|rest| rest.trim().eq_ignore_ascii_case("toml"))
        {
          start = Some(at);
        }
      }
      Some(begin) => {
        if t.starts_with("```") {
          let end = (at + line.trim_end_matches('\n').len() + 1).min(preamble.len());
          return Some((begin, end));
        }
      }
    }
    at += line.len();
  }
  None
}

fn merged_fence(preamble: &str, config: &SessionConfig) -> String {
  let fence = toml_fence(config);
  if fence_body(preamble).as_deref() == fence_body(&fence).as_deref() {
    return fence;
  }
  let Some(raw) = fence_body(preamble) else {
    return fence;
  };
  let Some(table) = raw.parse::<toml::Table>().ok() else {
    // A fence we cannot read is left exactly as written rather than replaced by our defaults.
    return String::new();
  };
  let modeled = fence_body(&fence)
    .and_then(|body| body.parse::<toml::Table>().ok())
    .unwrap_or_default();
  let extra: toml::Table = table
    .into_iter()
    .filter(|(k, _)| !modeled.contains_key(k))
    .collect();
  if extra.is_empty() {
    return fence;
  }
  let Ok(rendered) = toml::to_string(&extra) else {
    return fence;
  };
  let mut out = fence.trim_end_matches("```\n").to_string();
  out.push_str(&rendered);
  out.push_str("```\n");
  out
}

pub fn fence_body(preamble: &str) -> Option<String> {
  let mut inside = false;
  let mut body = String::new();
  for line in preamble.lines() {
    let t = line.trim();
    if !inside {
      if t
        .strip_prefix("```")
        .is_some_and(|rest| rest.trim().eq_ignore_ascii_case("toml"))
      {
        inside = true;
      }
      continue;
    }
    if t.starts_with("```") {
      return Some(body);
    }
    body.push_str(line);
    body.push('\n');
  }
  None
}

pub fn config_from_fence(preamble: &str) -> SessionConfig {
  fence_config(preamble).unwrap_or_default()
}

/// `None` means the fence is present but does not parse, which the caller must not paper over by
/// regenerating the fence: that would silently delete the human's settings.
fn fence_config(preamble: &str) -> Option<SessionConfig> {
  let body = fence_body(preamble)?;
  if body.trim().is_empty() {
    return Some(SessionConfig::default());
  }
  toml::from_str(&body).ok()
}

pub fn parse(text: &str) -> (Session, ParseReport) {
  let lines: Vec<&str> = text
    .split('\n')
    .map(|l| l.strip_suffix('\r').unwrap_or(l))
    .collect();

  // The fence is configuration, not structure: everything above the `begin` marker is the human's,
  // and a `<!-- slopfile:begin -->` line sitting inside the fence must not be mistaken for one.
  let after_fence = fence_end_line(&lines);
  let begin_ix = lines
    .iter()
    .enumerate()
    .skip(after_fence)
    .find(|(_, l)| open_marker(l).is_some_and(|(n, _)| n == BEGIN))
    .map(|(ix, _)| ix);
  let (preamble, start) = match begin_ix {
    Some(ix) => (lines[..ix].join("\n"), ix + 1),
    // No marker at all: the whole file is preamble, so a hand-written note is never destroyed.
    None => (lines.join("\n"), lines.len()),
  };

  let mut report = ParseReport::default();
  let mut stack: Vec<Node> = Vec::new();
  let mut roots: Vec<Node> = Vec::new();
  let mut open: Option<Vec<String>> = None;
  let mut input_consumed = false;

  let mut ix = start;
  while ix < lines.len() {
    let line = lines[ix];

    if is_close(line) {
      match stack.pop() {
        Some(node) => attach(&mut stack, &mut roots, node),
        None => report.duplicate_close = true,
      }
      ix += 1;
      continue;
    }

    if let Some((name, payload)) = open_marker(line) {
      if name == BEGIN {
        ix += 1;
        continue;
      }
      if name == INPUT {
        if stack.last().is_some_and(|n| !CONTAINER_REGIONS.contains(&n.name.as_str())) {
          let leaf = stack.pop().expect("open leaf");
          open = Some(open_path(&stack, &leaf.name));
          attach(&mut stack, &mut roots, leaf);
        } else if !stack.is_empty() {
          open = Some(open_path(&stack, ""));
        }
        // The input region is a root by definition: filing it under whatever container happens to
        // be open would hide the human's text inside that container's body and lose it.
        roots.push(Node {
          name: INPUT.to_string(),
          lines: lines[ix + 1..].iter().map(|l| l.to_string()).collect(),
          ..Node::default()
        });
        input_consumed = true;
        break;
      }
      if stack.last().is_some_and(|n| !CONTAINER_REGIONS.contains(&n.name.as_str())) {
        let after: Vec<String> = lines[ix..]
          .iter()
          .filter_map(|l| open_marker(l).map(|(n, _)| n.to_string()))
          .collect();
        if !after.is_empty() {
          let owner = stack
            .last()
            .map(|n| n.name.clone())
            .unwrap_or_default();
          report.swallowed.push(format!(
            "`{owner}` was left unclosed, so `{}` and {} more region(s) became plain text",
            after.join("`, `"),
            after.len().saturating_sub(1)
          ));
        }
        let node = stack.last_mut().expect("leaf node");
        node.lines.extend(lines[ix..].iter().map(|l| l.to_string()));
        open = Some(open_path(&stack, ""));
        break;
      }
      if !CONTAINER_REGIONS.contains(&name) && !LEAF_REGIONS.contains(&name) {
        report.unknown_regions.push(name.to_string());
      }
      stack.push(Node {
        name: name.to_string(),
        payload: payload.to_string(),
        ..Node::default()
      });
      ix += 1;
      continue;
    }

    if let Some(node) = stack.last_mut() {
      node.lines.push(line.to_string());
    }
    ix += 1;
  }

  if open.is_none() && !input_consumed && !stack.is_empty() {
    open = Some(open_of(&stack));
  }
  while let Some(node) = stack.pop() {
    attach(&mut stack, &mut roots, node);
  }

  let mut session = Session {
    preamble: preamble.clone(),
    config: config_from_fence(&preamble),
    open,
    ..Session::default()
  };
  let mut payload_problems = Vec::new();
  for root in roots.iter() {
    let root_path = vec![root.name.clone()];
    match root.name.as_str() {
      SYSTEM => session.system = unescape(&root.body()),
      TURN => session.turns.push(turn_of(root, &root_path, &mut payload_problems)),
      INPUT => session.input = root.body(),
      name => misplaced(
        &mut payload_problems,
        name,
        &root_path,
        "only `system`, `turn` and `input` may sit at the top level",
      ),
    }
  }
  report.bad_payloads = payload_problems;
  (session, report)
}

fn attach(stack: &mut Vec<Node>, roots: &mut Vec<Node>, node: Node) {
  match stack.last_mut() {
    Some(parent) => parent.children.push(node),
    None => roots.push(node),
  }
}

fn open_path(stack: &[Node], innermost: &str) -> Vec<String> {
  let mut path: Vec<String> = stack.iter().map(|n| n.name.clone()).collect();
  if !innermost.is_empty() {
    path.push(innermost.to_string());
  }
  path
}

fn under(path: &[String], name: &str) -> Vec<String> {
  let mut p = path.to_vec();
  p.push(name.to_string());
  p
}

fn turn_of(node: &Node, path: &[String], problems: &mut Vec<String>) -> Turn {
  let mut turn = Turn::default();
  let mut rounds: Vec<Round> = Vec::new();
  let mut current: Option<Round> = None;
  for child in node.children.iter() {
    match child.name.as_str() {
      USER if turn.user.is_some() => misplaced(problems, &child.name, path, "a second `user` region"),
      USER => turn.user = Some(unescape(&child.body())),
      NOTE => turn.notes.push(Note {
        by: note_by(&child.payload),
        text: unescape(&child.body()),
      }),
      ASSISTANT => {
        if let Some(round) = current.take() {
          rounds.push(round);
        }
        current = Some(Round {
          assistant: assistant_of(child, path, problems),
          results: Vec::new(),
        });
      }
      RESULT => {
        current
          .get_or_insert_with(Round::default)
          .results
          .push(outcome_of(child, path, problems));
      }
      QUESTION => {
        turn.question = decode_payload::<PendingQuestion>(
          QUESTION,
          &under(path, QUESTION),
          &child.payload,
          problems,
        );
      }
      name => misplaced(problems, name, path, "it does not belong inside a `turn`"),
    }
  }
  if let Some(round) = current {
    rounds.push(round);
  }
  turn.rounds = rounds;
  turn
}

fn assistant_of(node: &Node, path: &[String], problems: &mut Vec<String>) -> Assistant {
  let mut assistant = Assistant::default();
  for child in node.children.iter() {
    match child.name.as_str() {
      REASONING if assistant.reasoning.is_some() => {
        misplaced(problems, &child.name, path, "a second `reasoning` region");
      }
      TEXT if assistant.text.is_some() => {
        misplaced(problems, &child.name, path, "a second `text` region");
      }
      REASONING => assistant.reasoning = Some(unescape(&child.body())),
      TEXT => assistant.text = Some(unescape(&child.body())),
      CALL => {
        if let Some(call) =
          decode_payload::<Call>(CALL, &under(path, CALL), &child.payload, problems)
        {
          assistant.calls.push(call);
        }
      }
      name => misplaced(problems, name, path, "it does not belong inside an `assistant`"),
    }
  }
  assistant
}

/// A region somewhere it has no meaning would be dropped on the next render, so it is reported
/// instead: better a refused session than a silently shortened conversation.
fn misplaced(problems: &mut Vec<String>, name: &str, path: &[String], why: &str) {
  problems.push(format!(
    "`{name}` in {} is out of place: {why}",
    region_path(path)
  ));
}

fn outcome_of(node: &Node, path: &[String], problems: &mut Vec<String>) -> ToolOutcome {
  let where_ = under(path, RESULT);
  let head = decode_payload::<ResultHeader>(RESULT, &where_, &node.payload, problems);
  let Some(head) = head else {
    return ToolOutcome {
      id: String::new(),
      name: String::new(),
      ok: false,
      ms: 0,
      output: format!(
        "unreadable {} header, so this result cannot be trusted: {}",
        RESULT,
        unescape(&node.body())
      ),
    };
  };
  ToolOutcome {
    id: head.id,
    name: head.name,
    ok: head.ok,
    ms: head.ms,
    output: unescape(&node.body()),
  }
}

fn note_by(payload: &str) -> NoteBy {
  match serde_json::from_str::<serde_json::Value>(payload) {
    Ok(v) if v.get("by").and_then(serde_json::Value::as_str) == Some("harness") => NoteBy::Harness,
    _ => NoteBy::Model,
  }
}

impl Session {
  pub fn new(config: SessionConfig, system: impl Into<String>) -> Self {
    Self {
      preamble: toml_fence(&config),
      config,
      system: system.into(),
      turns: Vec::new(),
      input: String::new(),
      open: None,
    }
  }

  pub fn render(&self) -> String {
    let mut out = self.render_history();
    let open = self.open.as_deref().unwrap_or_default();
    let open_turn = open.first().is_some_and(|n| n == TURN);
    let open_assistant = open.get(1).is_some_and(|n| n == ASSISTANT);
    let open_section = self.section();
    for (ti, turn) in self.turns.iter().enumerate() {
      let open_here = open_turn && ti + 1 == self.turns.len();
      out.push_str(&open_line(TURN, None));
      if let Some(user) = &turn.user {
        out.push_str(&leaf(user, USER, None));
      }
      for note in &turn.notes {
        let payload = match note.by {
          NoteBy::Harness => Some(r#"{"by":"harness"}"#),
          NoteBy::Model => None,
        };
        out.push_str(&leaf(&note.text, NOTE, payload));
      }
      for (ri, round) in turn.rounds.iter().enumerate() {
        let open_asst = open_here && open_assistant && ri + 1 == turn.rounds.len();
        out.push_str(&open_line(ASSISTANT, None));
        if let Some(r) = &round.assistant.reasoning {
          out.push_str(&leaf_inner(
          r,
          REASONING,
          None,
          !(open_asst && open_section == Some(REASONING)),
        ));
        }
        if let Some(t) = &round.assistant.text {
          out.push_str(&leaf_inner(
            t,
            TEXT,
            None,
            !(open_asst && open_section == Some(TEXT)),
          ));
        }
        for call in &round.assistant.calls {
          out.push_str(&open_line(CALL, Some(&json(call))));
          out.push_str(&close_line());
        }
        if !open_asst {
          out.push_str(&close_line());
        }
        for result in &round.results {
          let head = ResultHeader {
            id: result.id.clone(),
            name: result.name.clone(),
            ok: result.ok,
            ms: result.ms,
          };
          out.push_str(&leaf(&result.output, RESULT, Some(&json(&head))));
        }
      }
      if let Some(pending) = &turn.question {
        out.push_str(&open_line(QUESTION, Some(&json(pending))));
        out.push_str(&close_line());
      }
      if !open_here {
        out.push_str(&close_line());
      }
    }
    out.push_str(&open_line(INPUT, None));
    out.push_str(&self.input);
    out
  }

  fn render_history(&self) -> String {
    let mut out = String::new();
    let preamble = self.merged_preamble();
    if !preamble.is_empty() {
      out.push_str(&preamble);
      if !out.ends_with('\n') {
        out.push('\n');
      }
    }
    out.push_str(&open_line(BEGIN, None));
    if !self.system.is_empty() {
      out.push_str(&leaf(&self.system, SYSTEM, None));
    }
    out
  }

  /// The fence is regenerated from `config` so programmatic config changes always land, but any
  /// key the harness does not model is carried over so a hand edit to the fence is not lost. Human
  /// lines around the fence are kept verbatim, and a file with no fence stays that way.
  fn merged_preamble(&self) -> String {
    let Some(bounds) = fence_bounds(&self.preamble) else {
      return self.preamble.clone();
    };
    let (start, end) = bounds;
    let head = &self.preamble[..start];
    let tail = &self.preamble[end..];
    let merged = merged_fence(&self.preamble, &self.config);
    // An unreadable fence must survive verbatim no matter what surrounds it: replacing or dropping
    // it would silently reset every per-session setting and reject the model's own later edits.
    if merged.is_empty() {
      return self.preamble.clone();
    }
    let mut out = String::with_capacity(head.len() + merged.len() + tail.len());
    out.push_str(head);
    out.push_str(&merged);
    out.push_str(tail);
    out
  }

  /// Rewind the open round to its start and hand back the partial output the model already wrote,
  /// so the next request continues from it instead of restarting the sentence.
  pub fn rewind_open_round(&mut self) -> (Option<String>, Option<String>) {
    let round = self.last_round();
    let reasoning = round.assistant.reasoning.take();
    let text = round.assistant.text.take();
    round.results.clear();
    round.assistant.calls.clear();
    self.open = open_assistant();
    (reasoning, text)
  }

  /// A copy in which every round has one unique id per call and exactly one result per call.
  pub fn paired(&self) -> Session {
    let mut s = self.clone();
    s.shorten_ids();
    for turn in &mut s.turns {
      for round in &mut turn.rounds {
        let (calls, results) =
          pair_calls_with_results(round.assistant.calls.clone(), round.results.clone());
        round.assistant.calls = calls;
        round.results = results;
      }
    }
    s
  }

  pub fn request_messages(&self) -> Vec<ChatMessage> {
    // A session read back from disk can hold a call with no result (the model pruned it) or a
    // result with no call. The pairing has to happen here too, or we build a request the server
    // rejects and burn the whole retry budget on it.
    let repaired = self.paired();
    let this = &repaired;
    let mut out: Vec<ChatMessage> = Vec::new();
    if !self.system.trim().is_empty() {
      out.push(ChatMessage::system(self.system.trim()));
    }
    for turn in &this.turns {
      if let Some(user) = turn.user.as_deref()
        && !user.trim().is_empty()
      {
        out.push(ChatMessage::user(user.trim()));
      }
      for note in &turn.notes {
        let text = note.text.trim();
        if text.is_empty() {
          continue;
        }
        out.push(match note.by {
          NoteBy::Harness => ChatMessage::user(text),
          NoteBy::Model => assistant_message(Some(text), None, Vec::new()),
        });
      }
      for round in turn.rounds.iter() {
        let mut calls = round.assistant.calls.clone();
        for result in &round.results {
          if !calls.iter().any(|c| c.id == result.id) {
            calls.push(Call {
              id: result.id.clone(),
              name: result.name.clone(),
              arguments: "{}".to_string(),
            });
          }
        }
        let text = round.assistant.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        let reasoning = round
          .assistant
          .reasoning
          .as_deref()
          .map(str::trim)
          .filter(|t| !t.is_empty());
        if text.is_none() && reasoning.is_none() && calls.is_empty() && round.results.is_empty() {
          continue;
        }
        out.push(assistant_message(
          text,
          reasoning,
          if calls.is_empty() { Vec::new() } else { calls },
        ));
        for result in &round.results {
          out.push(ChatMessage::tool(
            result.id.clone(),
            result.name.clone(),
            result.output.clone(),
          ));
        }
      }
    }
    if !self.input.trim().is_empty() {
      out.push(ChatMessage::user(self.input.trim()));
    }
    coalesce_assistants(out)
  }

  pub fn turn_in_progress(&self) -> bool {
    self
      .open
      .as_ref()
      .is_some_and(|path| path.first().is_some_and(|n| n == TURN))
  }

  pub fn dangling(&self) -> bool {
    self.open.is_some()
  }

  pub fn section(&self) -> Option<&'static str> {
    match self.open.as_deref().and_then(|path| path.last()) {
      Some(n) if n == REASONING => Some(REASONING),
      Some(n) if n == TEXT => Some(TEXT),
      _ => None,
    }
  }

  pub fn pending_question(&self) -> Option<&PendingQuestion> {
    self.turns.iter().rev().find_map(|t| t.question.as_ref())
  }

  pub fn has_input(&self) -> bool {
    !self.input.trim().is_empty()
  }

  pub fn last_turn_complete(&self) -> bool {
    !self.turn_in_progress()
  }

  /// True when the model owes the user a reply: a dangling stream, a user message with nothing
  /// after it, or tool results it has not reacted to yet.
  pub fn needs_generation(&self) -> bool {
    self.turn_in_progress() || self.turns.last().is_some_and(Turn::needs_reply)
  }

  /// True when nothing is pending anywhere: no open region, no typed input, no owed reply.
  pub fn is_finished(&self) -> bool {
    !self.dangling() && !self.has_input() && self.turns.last().is_some_and(Turn::is_complete)
  }

  pub fn promote_input(&mut self) -> bool {
    let text = self.input.trim().to_string();
    if text.is_empty() {
      return false;
    }
    self.input.clear();
    // Only a completely empty trailing turn can absorb the text; anything else already holds a
    // message the user typed, and overwriting it would delete their words.
    match self.turns.last_mut() {
      Some(turn) if turn.is_empty() => turn.user = Some(text),
      _ => self.turns.push(Turn {
        user: Some(text),
        ..Turn::default()
      }),
    }
    true
  }

  pub fn resume_round(&mut self) {
    let round = self.last_round();
    round.assistant.reasoning = None;
    round.assistant.text = None;
    round.results.clear();
    self.open = open_assistant();
  }

  pub fn begin_round(&mut self) {
    if self.turns.is_empty() {
      self.turns.push(Turn::default());
    }
    self
      .turns
      .last_mut()
      .expect("just pushed")
      .rounds
      .push(Round::default());
    self.open = open_assistant();
  }

  pub fn push_reasoning(&mut self, delta: &str) {
    self.push_section(REASONING, delta);
  }

  pub fn push_text(&mut self, delta: &str) {
    self.push_section(TEXT, delta);
  }

  fn push_section(&mut self, which: &'static str, delta: &str) {
    if delta.is_empty() {
      return;
    }
    if let Some(current) = self.section()
      && current != which
    {
      self.close_section();
    }
    let round = self.last_round();
    let slot = if which == REASONING {
      &mut round.assistant.reasoning
    } else {
      &mut round.assistant.text
    };
    match slot {
      Some(text) => text.push_str(delta),
      None => *slot = Some(delta.to_string()),
    }
    if self.section() != Some(which) {
      self.open = Some(vec![
        TURN.to_string(),
        ASSISTANT.to_string(),
        which.to_string(),
      ]);
    }
  }

  pub fn last_round(&mut self) -> &mut Round {
    if self.turns.is_empty() {
      self.turns.push(Turn::default());
    }
    let turn = self.turns.last_mut().expect("just ensured");
    if turn.rounds.is_empty() {
      turn.rounds.push(Round::default());
    }
    turn.rounds.last_mut().expect("just ensured")
  }

  pub fn close_section(&mut self) {
    if self.section().is_some() {
      self.open = open_assistant();
    }
  }

  pub fn finish_round(&mut self, calls: Vec<Call>, results: Vec<ToolOutcome>) {
    let round = self.last_round();
    let (calls, results) = pair_calls_with_results(calls, results);
    round.assistant.calls = calls;
    round.results = results;
    let _ = round;
    self.shorten_ids();
    self.close_section();
    self.open = None;
  }

  /// Replace the model's tool-call ids with short per-round sequential ones.
  ///
  /// Ids only ever have to pair a call with its result *inside one round*, and the request is
  /// rebuilt from this file every turn, so the model's 28-char ids buy nothing while costing
  /// about a quarter of every tool round (and they are stored twice, once per side). Renumbering
  /// here and in `paired()` keeps the file and the request in agreement.
  pub fn shorten_ids(&mut self) {
    for turn in &mut self.turns {
      for round in &mut turn.rounds {
        let mut map: Vec<(String, String)> = Vec::new();
        for call in &mut round.assistant.calls {
          call.id = intern_id(&call.id, &mut map);
        }
        for result in &mut round.results {
          result.id = intern_id(&result.id, &mut map);
        }
      }
    }
  }

  pub fn close_turn(&mut self) {
    self.close_section();
    self.open = None;
  }

  pub fn add_note(&mut self, by: NoteBy, text: impl Into<String>) {
    if self.turns.is_empty() {
      self.turns.push(Turn::default());
    }
    self.turns.last_mut().expect("turn").notes.push(Note {
      by,
      text: text.into(),
    });
  }

  pub fn add_result(&mut self, result: ToolOutcome) {
    if self.turns.is_empty() {
      self.turns.push(Turn::default());
    }
    let turn = self.turns.last_mut().expect("turn");
    if turn.rounds.is_empty() {
      turn.rounds.push(Round::default());
    }
    let round = turn.rounds.last_mut().expect("round");
    if !round.results.is_empty() {
      turn.rounds.push(Round {
        assistant: Assistant::default(),
        results: vec![result],
      });
      return;
    }
    round.results.push(result);
  }

  /// Asking a question is a tool call with a result that says it is waiting, so the call/result
  /// pairing the server requires holds from the moment the question is recorded.
  pub fn set_question(&mut self, pending: PendingQuestion) {
    if self.turns.is_empty() {
      self.turns.push(Turn::default());
    }
    self.turns.last_mut().expect("turn").question = Some(pending.clone());
    let round = self.last_round();
    if !round
      .assistant
      .calls
      .iter()
      .any(|c| c.id == pending.call.id)
    {
      round.assistant.calls.push(pending.call.clone());
    }
    let waiting = ToolOutcome {
      id: pending.call.id.clone(),
      name: pending.call.name.clone(),
      ok: true,
      ms: 0,
      output: AWAITING_ANSWER.to_string(),
    };
    match round.results.iter_mut().find(|r| r.id == waiting.id) {
      Some(slot) => *slot = waiting,
      None => round.results.push(waiting),
    }
  }

  pub fn answer_question(&mut self, answers: Vec<(u32, String)>) -> Result<(), String> {
    let at = self
      .turns
      .iter()
      .rposition(|t| t.question.is_some())
      .ok_or("no pending question")?;
    let pending = self.turns[at].question.take().expect("just found");
    let turn = &mut self.turns[at];
    let payload = json(&serde_json::json!({
      "answers": answers
        .iter()
        .map(|(id, a)| serde_json::json!({"id": id, "answer": a}))
        .collect::<Vec<_>>(),
    }));
    if turn.rounds.is_empty() {
      turn.rounds.push(Round::default());
    }
    let round = turn.rounds.last_mut().expect("just ensured");
    if !round
      .assistant
      .calls
      .iter()
      .any(|c| c.id == pending.call.id)
    {
      round.assistant.calls.push(pending.call.clone());
    }
    let answered = ToolOutcome {
      id: pending.call.id.clone(),
      name: pending.call.name.clone(),
      ok: true,
      ms: 0,
      output: payload,
    };
    match round
      .results
      .iter_mut()
      .find(|r| r.id == answered.id && r.output == AWAITING_ANSWER)
    {
      Some(slot) => *slot = answered,
      None => round.results.push(answered),
    }
    self.input.clear();
    Ok(())
  }

  pub fn model(&self) -> Option<&str> {
    self.config.model.as_deref()
  }
}

fn open_assistant() -> Option<Vec<String>> {
  Some(vec![TURN.to_string(), ASSISTANT.to_string()])
}

fn open_of(stack: &[Node]) -> Vec<String> {
  open_path(stack, "")
}

/// Two assistant messages in a row (a model note followed by a reply, or a turn whose user region
/// was pruned) are folded into one, because that sequence is not a valid conversation.
/// Every call must get exactly one result, with a unique id. A model that omits ids, repeats one,
/// or (via an early `break` such as `solve`) leaves calls unexecuted would otherwise produce a
/// message stream the server rejects.
/// Map an id onto `c1`, `c2`, ... within a round, reusing the mapping for ids already seen so a
/// call and its result keep agreeing.
fn intern_id(id: &str, map: &mut Vec<(String, String)>) -> String {
  if let Some((_, short)) = map.iter().find(|(orig, _)| orig == id) {
    return short.clone();
  }
  let short = format!("c{}", map.len() + 1);
  map.push((id.to_string(), short.clone()));
  short
}

fn pair_calls_with_results(
  calls: Vec<Call>,
  results: Vec<ToolOutcome>,
) -> (Vec<Call>, Vec<ToolOutcome>) {
  let mut calls = calls;
  let mut results = results;
  let mut taken: Vec<String> = Vec::new();
  for (ix, call) in calls.iter_mut().enumerate() {
    if call.id.trim().is_empty() || taken.contains(&call.id) {
      call.id = format!("{}-{}", call.name, ix);
    }
    taken.push(call.id.clone());
  }
  let mut seen: Vec<String> = Vec::new();
  for result in results.iter_mut() {
    if result.id.trim().is_empty() || seen.contains(&result.id) || !taken.contains(&result.id) {
      match taken.iter().find(|id| !seen.contains(id)) {
        Some(id) => result.id = id.clone(),
        None => continue,
      }
    }
    seen.push(result.id.clone());
  }
  for call in calls.iter() {
    if seen.contains(&call.id) {
      continue;
    }
    seen.push(call.id.clone());
    results.push(ToolOutcome {
      id: call.id.clone(),
      name: call.name.clone(),
      ok: false,
      ms: 0,
      output: format!(
        "`{}` was not executed: the model asked for something else in the same message",
        call.name
      ),
    });
  }
  (calls, results)
}

fn coalesce_assistants(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
  let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
  for message in messages {
    let merge = message.role == Role::Assistant
      && out
        .last()
        .is_some_and(|last: &ChatMessage| last.role == Role::Assistant);
    match out.last_mut() {
      Some(last) if merge => {
        let (Some(content), Some(extra)) = (last.content.as_deref(), message.content.as_deref())
        else {
          if let Some(extra) = message.content {
            last.content = Some(match last.content.take() {
              Some(existing) => format!("{existing}\n\n{extra}"),
              None => extra,
            });
          }
          if let Some(reasoning) = message.reasoning_content {
            last.reasoning_content = Some(match last.reasoning_content.take() {
              Some(existing) => format!("{existing}\n\n{reasoning}"),
              None => reasoning,
            });
          }
          if let Some(calls) = message.tool_calls {
            last.tool_calls.get_or_insert_default().extend(calls);
          }
          continue;
        };
        last.content = Some(format!("{content}\n\n{extra}"));
        if let Some(reasoning) = message.reasoning_content {
          last.reasoning_content = Some(match last.reasoning_content.take() {
            Some(existing) => format!("{existing}\n\n{reasoning}"),
            None => reasoning,
          });
        }
        if let Some(calls) = message.tool_calls {
          last.tool_calls.get_or_insert_default().extend(calls);
        }
      }
      _ => out.push(message),
    }
  }
  out
}

fn assistant_message(
  content: Option<&str>,
  reasoning: Option<&str>,
  calls: Vec<Call>,
) -> ChatMessage {
  ChatMessage {
    role: Role::Assistant,
    content: content.map(str::to_string),
    reasoning_content: reasoning.map(str::to_string),
    tool_calls: (!calls.is_empty()).then(|| {
      calls
        .into_iter()
        .map(|c| ToolCall::new(c.id, c.name, c.arguments))
        .collect()
    }),
    tool_call_id: None,
    name: None,
  }
}

fn open_line(name: &str, payload: Option<&str>) -> String {
  match payload {
    Some(p) => format!("{TOKEN_OPEN}{name} {p} -->\n"),
    None => format!("{TOKEN_OPEN}{name} -->\n"),
  }
}

fn close_line() -> String {
  format!("{TOKEN_CLOSE}\n")
}

fn leaf(body: &str, name: &str, payload: Option<&str>) -> String {
  leaf_inner(body, name, payload, true)
}

fn leaf_inner(body: &str, name: &str, payload: Option<&str>, closed: bool) -> String {
  let mut out = open_line(name, payload);
  let escaped = escape(body);
  if !escaped.is_empty() {
    out.push_str(&escaped);
    if !out.ends_with('\n') {
      out.push('\n');
    }
  }
  if closed {
    out.push_str(&close_line());
  }
  out
}

fn json<T: Serialize>(value: &T) -> String {
  serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;
