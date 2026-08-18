//! Transcript lexer: turns a session `.md` file into a typed token stream.
//!
//! The transcript grammar is line-oriented. Every meaningful line is
//! classified once, here, into a [`TokKind`]; consumers (the parser in
//! `transcript.rs`, the daemon in `main.rs`) then match on kinds instead of
//! re-scanning strings. Prose that merely *mentions* a marker is
//! `Text` — it can never trigger control logic.
//!
//! Fences: a ```` ```lang ```` ... ```` ``` ```` pair (with a closing fence
//! present) lexes its contents as plain `Text`. An *unclosed* opening fence
//! is itself `Text`: a streamed response is frequently truncated mid-fence,
//! and swallowing the rest of the file as literal text would hide real
//! markers behind it.

/// `<!-- @done -->`: a completed turn.
pub const DONE_MARKER: &str = "<!-- @done -->";
/// `<!-- slopfile:abort -->`: user abort request (control, never content).
pub const ABORT_MARKER: &str = "<!-- slopfile:abort -->";
pub const THINK_START: &str = "<!-- thinking -->";
pub const THINK_END: &str = "<!-- /thinking -->";
/// Prefix of the daemon status line (`<!-- slopfile:live {...} -->`).
pub const LIVE_PREFIX: &str = "<!-- slopfile:live";
/// Prefix of the daemon's context-full note.
pub const CONTEXT_FULL_PREFIX: &str = "<!-- slopfile:context-full";

/// One classified line. `text` is the raw line (no trailing newline,
/// original whitespace); `payload` is the trimmed inner content for
/// marker kinds (e.g. the tool-call JSON between `<!-- tool:` and ` -->`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
  pub kind: TokKind,
  /// 0-based line index in the source.
  pub line: usize,
  /// Byte offset of the line start in the source.
  pub offset: usize,
  pub text: &'a str,
  pub payload: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
  /// ```` ```lang ```` — only when a matching close fence follows.
  FenceOpen,
  /// ```` ``` ```` — closes the current fence.
  FenceClose,
  /// Markdown heading (`# `, `#### `, ...). Payload: the text after `#`.
  Heading,
  /// `<!-- @done -->`
  Done,
  /// `<!-- slopfile:abort -->` as a standalone line, or glued after the
  /// status line.
  Abort,
  /// `<!-- slopfile:live {...} -->` — daemon status; never user content.
  Live,
  /// `<!-- slopfile:context-full: ... -->` — daemon note.
  ContextFull,
  /// `<!-- thinking -->`
  ThinkingOpen,
  /// `<!-- /thinking -->`
  ThinkingEnd,
  /// `<!-- tool:NAME ARGS -->`
  Tool,
  /// `<!-- result:NAME ok=... -->`
  Result,
  /// `<!-- call:NAME ARGS -->` — the original question-tool call.
  Call,
  /// `<!-- q:ID {...} -->` — stashed question JSON.
  QuestionStash,
  /// `❓ QUESTIONS` — start of a QUESTIONS block.
  QuestionHeader,
  /// `> text` — the user's answer line below a QUESTIONS block.
  Answer,
  /// `/skip` or `> /skip`.
  Skip,
  /// Any other line.
  Text,
}

impl TokKind {
  /// Daemon-owned control lines: never user content, never LLM context.
  /// (The status line, the abort marker, the context-full note.)
  pub fn is_daemon_owned(self) -> bool {
    matches!(self, TokKind::Live | TokKind::Abort | TokKind::ContextFull)
  }
}

impl<'a> Token<'a> {
  /// Tool-ish markers: name = first word of the payload, args = the rest.
  pub fn name_args(&self) -> (&'a str, &'a str) {
    self
      .payload
      .split_once(' ')
      .map(|(n, a)| (n, a.trim()))
      .unwrap_or((self.payload, "{}"))
  }
}

/// Lex the transcript into a token stream. Fence-aware: once an opening
/// fence is seen and a matching close fence exists later, everything
/// between is `Text`.
pub fn lex(text: &str) -> Vec<Token<'_>> {
  let mut tokens = Vec::new();
  let mut in_fence = false;
  let mut offset = 0usize;
  for (line_ix, raw) in text.lines().enumerate() {
    let tok = classify(raw, in_fence);
    if in_fence {
      tokens.push(Token {
        kind: if matches!(tok.kind, TokKind::FenceClose) {
          TokKind::FenceClose
        } else {
          TokKind::Text
        },
        line: line_ix,
        offset,
        text: raw,
        payload: "",
      });
      if matches!(tok.kind, TokKind::FenceClose) {
        in_fence = false;
      }
      offset += raw.len() + 1;
      continue;
    }
    match tok.kind {
      TokKind::FenceOpen => {
        // Lookahead for a closing fence; without one the opener is a plain
        // Text line (typical for a stream cut mid-code-block).
        let close = text.lines().skip(line_ix + 1).any(|l| classify(l, false).kind == TokKind::FenceClose);
        if close {
          in_fence = true;
        }
        tokens.push(Token {
          kind: if close { TokKind::FenceOpen } else { TokKind::Text },
          line: line_ix,
          offset,
          text: raw,
          payload: if close { tok.payload } else { "" },
        });
      }
      _ => tokens.push(Token {
        kind: tok.kind,
        line: line_ix,
        offset,
        text: raw,
        payload: tok.payload,
      }),
    }
    offset += raw.len() + 1;
  }
  tokens
}

/// Classify a single line (no fence context). Used for the line-level
/// checks the daemon runs on streaming writes (abort marker detection).
pub fn lex_line(raw: &str) -> TokKind {
  classify(raw, false).kind
}

/// Classify a single line. Returns the kind and, for marker kinds, the
/// trimmed inner payload (between the `<!-- name:` and the terminating
/// ` -->`).
fn classify(raw: &str, in_fence: bool) -> TokenKindWithPayload<'_> {
  if in_fence {
    if raw.trim() == "```" {
      return token(TokKind::FenceClose, "");
    }
    return token(TokKind::Text, "");
  }
  let t = raw.trim();
  match t {
    _ if t == DONE_MARKER => token(TokKind::Done, ""),
    _ if t == THINK_START => token(TokKind::ThinkingOpen, ""),
    _ if t == THINK_END => token(TokKind::ThinkingEnd, ""),
    _ if t.starts_with("```") => {
      if t == "```" {
        token(TokKind::FenceClose, "")
      } else {
        token(TokKind::FenceOpen, t[3..].trim())
      }
    }
    _ if t.starts_with(ABORT_MARKER) || t.ends_with(ABORT_MARKER) => token(TokKind::Abort, ""),
    _ if t.starts_with(LIVE_PREFIX) => token(TokKind::Live, payload(t, LIVE_PREFIX)),
    _ if t.starts_with(CONTEXT_FULL_PREFIX) => token(TokKind::ContextFull, payload(t, CONTEXT_FULL_PREFIX)),
    _ if t.starts_with("<!-- tool:") => token(TokKind::Tool, payload(t, "<!-- tool:")),
    _ if t.starts_with("<!-- result:") => token(TokKind::Result, payload(t, "<!-- result:")),
    _ if t.starts_with("<!-- call:") => token(TokKind::Call, payload(t, "<!-- call:")),
    _ if t.starts_with("<!-- q:") => token(TokKind::QuestionStash, payload(t, "<!-- q:")),
    _ if t.starts_with("> /skip") => token(TokKind::Skip, ""),
    _ if t == "/skip" => token(TokKind::Skip, ""),
    _ if t.starts_with('>') => token(TokKind::Answer, t[1..].trim()),
    _ if t.starts_with('#') => token(TokKind::Heading, t.trim_start_matches('#').trim()),
    _ if t.starts_with("❓ QUESTIONS") => token(TokKind::QuestionHeader, ""),
    _ => token(TokKind::Text, ""),
  }
}

/// Inner payload of `<!-- name: ... -->` markers — the text between the
/// `name:` and the first ` -->`.
fn payload<'a>(t: &'a str, prefix: &str) -> &'a str {
  let rest = &t[prefix.len()..];
  match rest.find(" -->") {
    Some(ix) => rest[..ix].trim(),
    None => rest.trim(),
  }
}

struct TokenKindWithPayload<'a> {
  kind: TokKind,
  payload: &'a str,
}

fn token<'a>(kind: TokKind, payload: &'a str) -> TokenKindWithPayload<'a> {
  TokenKindWithPayload { kind, payload }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn kinds(text: &str) -> Vec<TokKind> {
    lex(text).iter().map(|t| t.kind).collect()
  }

  #[test]
  fn markers_lex_to_their_kinds() {
    let text = "\
<!-- @done -->
<!-- slopfile:abort -->
<!-- slopfile:live {\"phase\":\"idle\"} -->
<!-- thinking -->
plain text
### You
> 1, 2
/skip
> /skip
❓ QUESTIONS
<!-- tool:bash {\"cmd\":\"ls\"} -->
<!-- result:bash ok=3ms -->
<!-- call:question {\"a\":1} -->
<!-- q:1 {\"question\":\"x\"} -->
# Session
text with <!-- @done --> inside
";
    assert_eq!(
      kinds(text),
      vec![
        TokKind::Done,
        TokKind::Abort,
        TokKind::Live,
        TokKind::ThinkingOpen,
        TokKind::Text,
        TokKind::Heading,
        TokKind::Answer,
        TokKind::Skip,
        TokKind::Skip,
        TokKind::QuestionHeader,
        TokKind::Tool,
        TokKind::Result,
        TokKind::Call,
        TokKind::QuestionStash,
        TokKind::Heading,
        TokKind::Text,
      ]
    );
  }

  #[test]
  fn prose_mentioning_a_marker_is_text_not_abort() {
    let text = "47: - Abort: `<!-- slopfile:abort -->` marker line appended by `:Abort`\n";
    assert_eq!(kinds(text), vec![TokKind::Text]);
  }

  #[test]
  fn abort_glued_to_status_line_is_an_abort() {
    let text = "<!-- slopfile:live {\"phase\":\"generating\"} --><!-- slopfile:abort -->\n";
    assert_eq!(kinds(text), vec![TokKind::Abort]);
  }

  #[test]
  fn fence_region_is_all_text() {
    let text = "\
```toml
model = \"m\"
```
### You
hi
";
    assert_eq!(
      kinds(text),
      vec![
        TokKind::FenceOpen,
        TokKind::Text,
        TokKind::FenceClose,
        TokKind::Heading,
        TokKind::Text,
      ]
    );
  }

  #[test]
  fn unclosed_fence_is_text_and_does_not_swallow() {
    let text = "\
### Assistant
```json
{\"x\": 1}
<!-- tool:bash {\"cmd\":\"ls\"} -->
";
    // No closing fence: the opener must not swallow the markers after it.
    assert_eq!(
      kinds(text),
      vec![TokKind::Heading, TokKind::Text, TokKind::Text, TokKind::Tool]
    );
  }

  #[test]
  fn closed_fence_keeps_inner_markers_hidden() {
    let text = "\
### Assistant
```md
### You
<!-- @done -->
```
";
    assert_eq!(
      kinds(text),
      vec![
        TokKind::Heading,
        TokKind::FenceOpen,
        TokKind::Text,
        TokKind::Text,
        TokKind::FenceClose,
      ]
    );
  }

  #[test]
  fn offsets_track_line_starts() {
    let text = "ab\ncd\n";
    let toks = lex(text);
    assert_eq!(toks[0].offset, 0);
    assert_eq!(toks[0].line, 0);
    assert_eq!(toks[1].offset, 3);
    assert_eq!(toks[1].line, 1);
  }

  #[test]
  fn name_args_split() {
    let toks = lex("<!-- tool:read {\"path\":\"a\"} -->\n");
    let (name, args) = toks[0].name_args();
    assert_eq!(name, "read");
    assert_eq!(args, "{\"path\":\"a\"}");
    let toks = lex("<!-- tool:bash -->\n");
    let (name, args) = toks[0].name_args();
    assert_eq!(name, "bash");
    assert_eq!(args, "{}");
  }

  #[test]
  fn daemon_owned_is_exactly_abort_live_contextfull() {
    assert!(TokKind::Abort.is_daemon_owned());
    assert!(TokKind::Live.is_daemon_owned());
    assert!(TokKind::ContextFull.is_daemon_owned());
    assert!(!TokKind::Tool.is_daemon_owned());
    assert!(!TokKind::Result.is_daemon_owned());
    assert!(!TokKind::Text.is_daemon_owned());
  }
}