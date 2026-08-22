
pub const DONE_MARKER: &str = "<!-- @done -->";
pub const SEND_MARKER: &str = "<!-- slopfile:send -->";
pub const ABORT_MARKER: &str = "<!-- slopfile:abort -->";
pub const ABORTED_MARKER: &str = "<!-- slopfile:aborted -->";
pub const THINK_START: &str = "<!-- thinking -->";
pub const THINK_END: &str = "<!-- /thinking -->";
pub const CONTEXT_FULL_PREFIX: &str = "<!-- slopfile:context-full";
pub const SOLVED_MARKER: &str = "<!-- slopfile:solved -->";
pub const CONTEXT_FULL_NOTE: &str = "<!-- slopfile:context-full: the model hit the token cap mid-response; trim the useless old turns above, then write your next message below this note -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
  pub kind: TokKind,
  pub line: usize,
  pub offset: usize,
  pub text: &'a str,
  pub payload: &'a str,
  pub eol_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
  FenceOpen,
  FenceClose,
  Heading,
  Done,
  Send,
  Abort,
  Aborted,
  Live,
  ContextFull,
  Solved,
  ThinkingOpen,
  ThinkingEnd,
  Tool,
  Result,
  Call,
  QuestionStash,
  QuestionHeader,
  Answer,
  Skip,
  Text,
}

impl TokKind {
  pub fn is_daemon_owned(self) -> bool {
    matches!(
      self,
      TokKind::Live
        | TokKind::Send
        | TokKind::Abort
        | TokKind::Aborted
        | TokKind::ContextFull
        | TokKind::Solved
    )
  }

  pub fn is_turn_end(self) -> bool {
    matches!(self, TokKind::Done | TokKind::Aborted)
  }
}

impl<'a> Token<'a> {
  pub fn name_args(&self) -> (&'a str, &'a str) {
    self
      .payload
      .split_once(' ')
      .map(|(n, a)| (n, a.trim()))
      .unwrap_or((self.payload, "{}"))
  }

  pub fn line_end(&self) -> usize {
    self.offset + self.text.len() + self.eol_len
  }

  pub fn is_skip(&self) -> bool {
    self.kind == TokKind::Skip || (self.kind == TokKind::Answer && self.payload == "/skip")
  }
}

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

pub fn lex_line(raw: &str) -> TokKind {
  classify(raw).kind
}

fn fence_boundary(line: &str) -> bool {
  let t = line.trim();
  t == DONE_MARKER || t == ABORTED_MARKER || t == "### You" || t == "### Assistant"
}

fn backtick_run(line: &str) -> Option<usize> {
  let t = line.trim();
  let run = t.bytes().take_while(|&b| b == b'`').count();
  if run > 0 && t[run..].trim().is_empty() {
    Some(run)
  } else {
    None
  }
}

fn classify(raw: &str) -> TokenKindWithPayload<'_> {
  let t = raw.trim();
  match t {
    _ if t == DONE_MARKER => token(TokKind::Done, ""),
    _ if t == SEND_MARKER => token(TokKind::Send, ""),
    _ if t == THINK_START => token(TokKind::ThinkingOpen, ""),
    _ if t == THINK_END => token(TokKind::ThinkingEnd, ""),
    _ if backtick_run(t).is_some() => token(TokKind::FenceClose, ""),
    _ if t.starts_with("```") => {
      let run = t.bytes().take_while(|&b| b == b'`').count();
      token(TokKind::FenceOpen, t[run..].trim())
    }
    _ if t == ABORT_MARKER
      || (t.starts_with("<!-- slopfile:live ") && t.ends_with(ABORT_MARKER)) =>
    {
      token(TokKind::Abort, "")
    }
    _ if t == ABORTED_MARKER => token(TokKind::Aborted, ""),
    _ if t == SOLVED_MARKER => token(TokKind::Solved, ""),
    _ if t.starts_with("<!-- slopfile:live ") && t.ends_with("-->") => {
      token(TokKind::Live, closing_payload(t, "<!-- slopfile:live "))
    }
    _ if t.starts_with(CONTEXT_FULL_PREFIX)
      && matches!(t.as_bytes().get(CONTEXT_FULL_PREFIX.len()), Some(b' ' | b':'))
      && t.ends_with("-->") =>
    {
      token(TokKind::ContextFull, closing_payload(t, CONTEXT_FULL_PREFIX))
    }
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
    _ if t == "/skip" || (t.starts_with('>') && t[1..].trim() == "/skip") => {
      token(TokKind::Skip, "")
    }
    _ if t.starts_with('>') => token(TokKind::Answer, t[1..].trim()),
    _ if t.starts_with('#') => token(TokKind::Heading, t.trim_start_matches('#').trim()),
    _ if t.starts_with("QUESTIONS") => token(TokKind::QuestionHeader, ""),
    _ => token(TokKind::Text, ""),
  }
}

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
QUESTIONS
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
    assert_eq!(kinds("> <!-- slopfile:abort -->\n"), vec![TokKind::Answer]);
    assert_eq!(
      kinds("<!-- slopfile:abort --> please stop\n"),
      vec![TokKind::Text]
    );
    assert_eq!(kinds("<!-- slopfile:abort\n"), vec![TokKind::Text]);
    assert_eq!(kinds("notes: <!-- slopfile:abort -->\n"), vec![TokKind::Text]);
  }

  #[test]
  fn aborted_marker_is_a_distinct_turn_end() {
    assert_eq!(kinds("<!-- slopfile:aborted -->\n"), vec![TokKind::Aborted]);
    let toks = lex("a\n<!-- slopfile:aborted -->\nb\n");
    assert!(toks.iter().any(|t| t.kind == TokKind::Aborted));
    assert_eq!(kinds("notes: <!-- slopfile:aborted -->\n"), vec![TokKind::Text]);
    assert!(TokKind::Aborted.is_daemon_owned());
    assert!(TokKind::Aborted.is_turn_end());
    assert!(TokKind::Done.is_turn_end());
    assert!(!TokKind::Abort.is_turn_end());
    assert!(!TokKind::Text.is_turn_end());
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
    let text = "ab\r\nQUESTIONS\r\ncd\r\nef";
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