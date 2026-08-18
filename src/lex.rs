//! Transcript lexer: turns a session `.md` file into a typed token stream.
//!
//! The transcript grammar is line-oriented. Every meaningful line is
//! classified once, here, into a [`TokKind`]; consumers (the parser in
//! `transcript.rs`, the daemon in `main.rs`) then match on kinds instead of
//! re-scanning strings. Prose that merely *mentions* a marker is
//! `Text` — it can never trigger control logic.
//!
//! Fences: a ```` ```lang ```` ... ```` ``` ```` pair (with a matching close
//! fence present before any structural boundary) lexes its contents as plain
//! `Text`. An *unclosed* opening fence is itself `Text`: a streamed response
//! is frequently truncated mid-fence, and swallowing the rest of the file as
//! literal text would hide real markers behind it. The close must match the
//! opener's backtick-run width (`````` ``` ```` can only be closed by a
//! `>= 4` backtick line).

/// `<!-- @done -->`: a completed turn.
pub const DONE_MARKER: &str = "<!-- @done -->";
/// `<!-- slopfile:abort -->`: user abort request (control, never content).
pub const ABORT_MARKER: &str = "<!-- slopfile:abort -->";
pub const THINK_START: &str = "<!-- thinking -->";
pub const THINK_END: &str = "<!-- /thinking -->";
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
  /// Length of the line's line ending in bytes (1 for `\n`, 2 for `\r\n`,
  /// 0 for a final unterminated line).
  pub eol_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
  /// ```` ```lang ```` — only when a matching close fence follows.
  FenceOpen,
  /// A pure-backtick line closing the current fence (run >= opener's run).
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

  /// Byte offset just past the complete raw line (text + line ending).
  pub fn line_end(&self) -> usize {
    self.offset + self.text.len() + self.eol_len
  }
}

/// Lex the transcript into a token stream. Fence-aware: once an opening
/// fence is seen and a matching close fence exists before the next
/// structural boundary, everything between is `Text`. Byte offsets are
/// exact for both `\n` and `\r\n` line endings and for multi-byte UTF-8.
pub fn lex(text: &str) -> Vec<Token<'_>> {
  let chunks: Vec<&str> = text.split_inclusive('\n').collect();
  let mut tokens = Vec::new();
  let mut in_fence = false;
  let mut fence_run = 0usize;
  let mut offset = 0usize;
  for (line_ix, chunk) in chunks.iter().enumerate() {
    let raw = chunk.trim_end_matches(['\r', '\n']);
    let eol_len = chunk.len() - raw.len();
    if in_fence {
      let closes = backtick_run(raw).is_some_and(|r| r >= fence_run);
      tokens.push(Token {
        kind: if closes { TokKind::FenceClose } else { TokKind::Text },
        line: line_ix,
        offset,
        text: raw,
        payload: "",
        eol_len,
      });
      if closes {
        in_fence = false;
      }
      offset += chunk.len();
      continue;
    }
    let tok = classify(raw);
    match tok.kind {
      TokKind::FenceOpen => {
        // Lookahead for a closing fence within the same turn: closing is
        // only considered up to a structural boundary line (a `@done`
        // marker or an exact `### You` / `### Assistant` heading). Without
        // a close in the window the opener is a plain Text line (typical
        // for a stream cut mid-code-block).
        let run = raw.trim().bytes().take_while(|&b| b == b'`').count();
        let close = chunks[line_ix + 1..]
          .iter()
          .map(|c| c.trim_end_matches(['\r', '\n']))
          .take_while(|l| !fence_boundary(l))
          .any(|l| backtick_run(l).is_some_and(|r| r >= run));
        if close {
          in_fence = true;
          fence_run = run;
        }
        tokens.push(Token {
          kind: if close { TokKind::FenceOpen } else { TokKind::Text },
          line: line_ix,
          offset,
          text: raw,
          payload: if close { tok.payload } else { "" },
          eol_len,
        });
      }
      _ => tokens.push(Token {
        kind: tok.kind,
        line: line_ix,
        offset,
        text: raw,
        payload: tok.payload,
        eol_len,
      }),
    }
    offset += chunk.len();
  }
  tokens
}

/// Classify a single line (no fence context). Used for the line-level
/// checks the daemon runs on streaming writes (abort marker detection).
pub fn lex_line(raw: &str) -> TokKind {
  classify(raw).kind
}

/// Structural lines a fence may never span: a `@done` marker line or an
/// exact `### You` / `### Assistant` section heading.
fn fence_boundary(line: &str) -> bool {
  let t = line.trim();
  t == DONE_MARKER || t == "### You" || t == "### Assistant"
}

/// Length of the leading backtick run of `line` when the whole trimmed
/// line is pure backticks (a closing fence), `None` otherwise.
fn backtick_run(line: &str) -> Option<usize> {
  let t = line.trim();
  let run = t.bytes().take_while(|&b| b == b'`').count();
  if run > 0 && t[run..].trim().is_empty() {
    Some(run)
  } else {
    None
  }
}

/// Classify a single line. Returns the kind and, for marker kinds, the
/// trimmed inner payload (between the `<!-- name:` and the terminating
/// ` -->`).
fn classify(raw: &str) -> TokenKindWithPayload<'_> {
  let t = raw.trim();
  match t {
    _ if t == DONE_MARKER => token(TokKind::Done, ""),
    _ if t == THINK_START => token(TokKind::ThinkingOpen, ""),
    _ if t == THINK_END => token(TokKind::ThinkingEnd, ""),
    _ if backtick_run(t).is_some() => token(TokKind::FenceClose, ""),
    _ if t.starts_with("```") => {
      let run = t.bytes().take_while(|&b| b == b'`').count();
      token(TokKind::FenceOpen, t[run..].trim())
    }
    // Standalone abort marker, or the abort marker glued directly after the
    // daemon status line. The status-line prefix requires the trailing
    // space (same rule as below), so `<!-- slopfile:liveness ... -->` can
    // never masquerade as a status line.
    _ if t == ABORT_MARKER
      || (t.starts_with("<!-- slopfile:live ") && t.ends_with(ABORT_MARKER)) =>
    {
      token(TokKind::Abort, "")
    }
    // The daemon status line: `<!-- slopfile:live ` + payload + ` -->`.
    // Requires the trailing space: `<!-- slopfile:liveness ... -->` is
    // user text, never a status line.
    _ if t.starts_with("<!-- slopfile:live ") && t.ends_with("-->") => {
      token(TokKind::Live, closing_payload(t, "<!-- slopfile:live "))
    }
    // The daemon's context-full note: `<!-- slopfile:context-full` followed
    // by a space or `:` and closed with `-->`. `context-fullness ...` is
    // user text, never the note.
    _ if t.starts_with(CONTEXT_FULL_PREFIX)
      && matches!(t.as_bytes().get(CONTEXT_FULL_PREFIX.len()), Some(b' ' | b':'))
      && t.ends_with("-->") =>
    {
      token(TokKind::ContextFull, closing_payload(t, CONTEXT_FULL_PREFIX))
    }
    // Tool-ish markers only classify when the line actually closes with
    // `-->`; otherwise they are plain text (e.g. a stream cut mid-marker).
    _ if t.starts_with("<!-- tool:") && t.ends_with("-->") => {
      token(TokKind::Tool, closing_payload(t, "<!-- tool:"))
    }
    _ if t.starts_with("<!-- result:") && t.ends_with("-->") => {
      token(TokKind::Result, closing_payload(t, "<!-- result:"))
    }
    _ if t.starts_with("<!-- call:") && t.ends_with("-->") => {
      token(TokKind::Call, closing_payload(t, "<!-- call:"))
    }
    _ if t.starts_with("<!-- q:") && t.ends_with("-->") => {
      token(TokKind::QuestionStash, closing_payload(t, "<!-- q:"))
    }
    // `/skip` on its own line or as the whole text of an answer line.
    // `> /skippy boots` is an answer, not a skip.
    _ if t == "/skip" || (t.starts_with('>') && t[1..].trim() == "/skip") => {
      token(TokKind::Skip, "")
    }
    _ if t.starts_with('>') => token(TokKind::Answer, t[1..].trim()),
    _ if t.starts_with('#') => token(TokKind::Heading, t.trim_start_matches('#').trim()),
    _ if t.starts_with("❓ QUESTIONS") => token(TokKind::QuestionHeader, ""),
    _ => token(TokKind::Text, ""),
  }
}

/// Inner payload of a `<!-- name: ... -->` marker that is known to end
/// with `-->`: everything between the prefix and the closing `-->`,
/// trimmed. (The payload may itself contain `-->`, e.g. inside tool args.)
fn closing_payload<'a>(t: &'a str, prefix: &str) -> &'a str {
  t[prefix.len()..].strip_suffix("-->").unwrap_or("").trim()
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
  fn abort_requires_standalone_marker_or_glued_to_live() {
    assert_eq!(kinds("<!-- slopfile:abort -->\n"), vec![TokKind::Abort]);
    // Spoofs and partial streamed markers stay inert.
    assert_eq!(kinds("> <!-- slopfile:abort -->\n"), vec![TokKind::Answer]);
    assert_eq!(
      kinds("<!-- slopfile:abort --> please stop\n"),
      vec![TokKind::Text]
    );
    assert_eq!(kinds("<!-- slopfile:abort\n"), vec![TokKind::Text]);
    assert_eq!(kinds("notes: <!-- slopfile:abort -->\n"), vec![TokKind::Text]);
  }

  #[test]
  fn live_and_context_full_need_their_delimiters() {
    assert_eq!(kinds("<!-- slopfile:liveness is false -->\n"), vec![TokKind::Text]);
    assert_eq!(kinds("<!-- slopfile:live {\"x\":1}\n"), vec![TokKind::Text]);
    assert_eq!(
      kinds("<!-- slopfile:live {\"phase\":\"idle\"} -->\n"),
      vec![TokKind::Live]
    );
    assert_eq!(kinds("<!-- slopfile:context-fullness -->\n"), vec![TokKind::Text]);
    assert_eq!(
      kinds("<!-- slopfile:context-full: the model hit the cap; trim -->\n"),
      vec![TokKind::ContextFull]
    );
    assert_eq!(
      kinds("<!-- slopfile:context-full apples -->\n"),
      vec![TokKind::ContextFull]
    );
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
<!-- tool:bash {\"cmd\":\"ls\"} -->
plain
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
  fn retroactive_fence_stops_at_structural_boundaries() {
    // An unclosed fence must not reach across a `@done` / `### You` /
    // `### Assistant` boundary to close itself in the next turn.
    let text = "\
### Assistant
```json
{\"x\": 1}
<!-- @done -->
### You
next
### Assistant
```md
closed
```
";
    assert_eq!(
      kinds(text),
      vec![
        TokKind::Heading,
        TokKind::Text,
        TokKind::Text,
        TokKind::Done,
        TokKind::Heading,
        TokKind::Text,
        TokKind::Heading,
        TokKind::FenceOpen,
        TokKind::Text,
        TokKind::FenceClose,
      ]
    );
  }

  #[test]
  fn fence_closes_only_with_matching_or_longer_run() {
    let text = "````rust\nlet x = 1\n````\n";
    assert_eq!(
      kinds(text),
      vec![TokKind::FenceOpen, TokKind::Text, TokKind::FenceClose]
    );
    // A 3-backtick line cannot close a 4-backtick fence: no close in the
    // window, so the opener is Text.
    let text2 = "````rust\nlet x = 1\n```\n";
    assert_eq!(
      kinds(text2),
      vec![TokKind::Text, TokKind::Text, TokKind::FenceClose]
    );
  }

  #[test]
  fn fence_close_with_trailing_whitespace_still_closes() {
    let text = "```rust\ncode\n```   \n";
    assert_eq!(
      kinds(text),
      vec![TokKind::FenceOpen, TokKind::Text, TokKind::FenceClose]
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
  fn tool_args_survive_marker_inside() {
    // The payload must not truncate at the first `-->` inside the args.
    let toks = lex("<!-- tool:bash {\"cmd\":\"echo a --> b\"} -->\n");
    let (name, args) = toks[0].name_args();
    assert_eq!(name, "bash");
    assert_eq!(args, "{\"cmd\":\"echo a --> b\"}");
  }

  #[test]
  fn markers_without_closing_are_plain_text() {
    for line in [
      "<!-- tool:bash {\"cmd\":\"ls\"}",
      "<!-- result:bash ok=1ms",
      "<!-- call:question {}",
      "<!-- q:1 {}",
    ] {
      assert_eq!(kinds(&format!("{line}\n")), vec![TokKind::Text], "line: {line}");
    }
  }

  #[test]
  fn skip_requires_the_exact_command() {
    assert_eq!(kinds("/skip\n"), vec![TokKind::Skip]);
    assert_eq!(kinds("> /skip\n"), vec![TokKind::Skip]);
    assert_eq!(kinds("> /skippy boots\n"), vec![TokKind::Answer]);
    let toks = lex("> /skippy boots\n");
    assert_eq!(toks[0].payload, "/skippy boots");
  }

  #[test]
  fn crlf_offsets_are_exact_and_never_panic() {
    let text = "ab\r\n❓ QUESTIONS\r\ncd\r\nef";
    let toks = lex(text);
    assert_eq!(toks[0].offset, 0);
    assert_eq!(toks[0].eol_len, 2);
    assert_eq!(toks[0].line_end(), 4);
    assert_eq!(toks[1].kind, TokKind::QuestionHeader);
    assert_eq!(toks[1].offset, 4);
    assert_eq!(toks[2].offset, toks[1].line_end());
    assert_eq!(toks[2].eol_len, 2);
    assert_eq!(toks[3].eol_len, 0);
    assert_eq!(toks[3].line_end(), text.len());
    // Slicing back out of the source must roundtrip exactly.
    for t in &toks {
      assert_eq!(&text[t.offset..t.offset + t.text.len()], t.text);
      assert_eq!(t.line_end(), t.offset + t.text.len() + t.eol_len);
    }
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