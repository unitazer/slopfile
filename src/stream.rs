use crate::transcript::{reassemble, strip_live_line, without_abort_lines};
use crate::types::StreamEvent;
use std::path::Path;
use tracing::warn;

/// Streams assistant output into the transcript file below the current
/// section. Aborts are handled by the caller's watchdog (`keep_going` in
/// `LlamaClient::consume_stream`); finalization strips any abort lines that
/// landed mid-stream.
pub struct StreamWriter<'a> {
  path: &'a Path,
  out: String,
  live: Option<String>,
  wrote_header: bool,
  in_reasoning: bool,
}

impl<'a> StreamWriter<'a> {
  pub fn new(path: &'a Path, body: &str, live: Option<String>) -> Self {
    Self {
      path,
      out: body.to_string(),
      live,
      wrote_header: false,
      in_reasoning: false,
    }
  }

  fn ensure_header(&mut self) {
    let tail = self.out.trim_end();
    if tail == "### Assistant" || tail.ends_with("\n### Assistant") {
      self.wrote_header = true;
      if !self.out.ends_with('\n') {
        self.out.push('\n');
      }
      return;
    }
    if !self.wrote_header {
      if !self.out.is_empty() && !self.out.ends_with('\n') {
        self.out.push('\n');
      }
      self.out.push_str("### Assistant\n");
      self.wrote_header = true;
      self.flush();
    }
  }

  pub fn handle(&mut self, ev: &StreamEvent) -> bool {
    match ev {
      StreamEvent::Text(t) => {
        self.ensure_header();
        self.write_tag_aware(t);
      }
      StreamEvent::Reasoning(r) => {
        self.ensure_header();
        self.emit(r, true);
      }
      StreamEvent::ToolCallFragment(_) | StreamEvent::Done(_) => {}
    }
    true
  }

  fn emit(&mut self, s: &str, reasoning: bool) {
    if reasoning {
      if !self.in_reasoning {
        self.in_reasoning = true;
        self.out.push_str("<!-- thinking -->\n");
      }
    } else if self.in_reasoning {
      self.in_reasoning = false;
      self.out.push_str("<!-- /thinking -->\n");
    }
    if !s.is_empty() {
      self.out.push_str(s);
    }
    self.flush();
  }

  /// Models like Qwen3 close their native reasoning with literal
  /// `<think>`/`</think>` tags that llama-server can leak into the text
  /// channel (typically when resuming a cut-off reasoning block). Translate
  /// them to harness markers; an orphan `</think>` with no matching open
  /// state is dropped instead of polluting the transcript.
  fn write_tag_aware(&mut self, mut s: &str) {
    loop {
      let open = s.find("<think>");
      let close = s.find("</think>");
      let pos = match (open, close) {
        (None, None) => {
          if !s.is_empty() {
            self.emit(s, self.in_reasoning);
          }
          return;
        }
        (o, c) => match (o, c) {
          (Some(o), Some(c)) => o.min(c),
          (Some(o), None) => o,
          (None, Some(c)) => c,
          _ => unreachable!(),
        },
      };
      let (before, rest) = s.split_at(pos);
      if !before.is_empty() {
        self.emit(before, self.in_reasoning);
      }
      if let Some(open_rest) = rest.strip_prefix("<think>") {
        s = open_rest;
        self.emit("", true);
      } else {
        s = rest.strip_prefix("</think>").expect("matched earlier");
        self.emit("", false);
      }
    }
  }

  fn flush(&mut self) {
    let out = reassemble(&self.out, "", self.live.as_deref());
    if let Err(e) = std::fs::write(self.path, out) {
      warn!("stream write failed: {e}");
    }
  }

  /// Close the reasoning block and write the settled section state.
  pub fn finish_section(&self) {
    let (body, _) = strip_live_line(&self.out);
    let mut extra = String::new();
    if self.in_reasoning {
      extra.push_str("<!-- /thinking -->\n");
    }
    let clean = without_abort_lines(&reassemble(&body, &extra, self.live.as_deref()));
    let _ = std::fs::write(self.path, clean);
  }
}
