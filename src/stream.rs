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
        if self.in_reasoning {
          self.in_reasoning = false;
          self.out.push_str("<!-- /thinking -->\n");
        }
        self.out.push_str(t);
        self.flush();
      }
      StreamEvent::Reasoning(r) => {
        self.ensure_header();
        if !self.in_reasoning {
          self.in_reasoning = true;
          self.out.push_str("<!-- thinking -->\n");
        }
        self.out.push_str(r);
        self.flush();
      }
      StreamEvent::ToolCallFragment(_) | StreamEvent::Done(_) => {}
    }
    true
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
