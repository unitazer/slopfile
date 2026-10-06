use crate::session::{Session, REASONING, TEXT};
use crate::transcript;
use crate::types::StreamEvent;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::warn;

const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

pub struct StreamWriter {
  path: PathBuf,
  session: Session,
  last_flush: Instant,
}

impl StreamWriter {
  pub fn new(path: &Path, session: Session) -> Self {
    Self {
      path: path.to_path_buf(),
      session,
      last_flush: Instant::now(),
    }
  }

  pub fn handle(&mut self, ev: &StreamEvent) {
    match ev {
      StreamEvent::Text(t) => self.write_tag_aware(t, false),
      StreamEvent::Reasoning(r) => self.emit(r, REASONING),
      StreamEvent::ToolCallFragment(_) | StreamEvent::Done(_) => {}
    }
  }

  fn emit(&mut self, s: &str, which: &'static str) {
    if !s.is_empty() {
      if which == REASONING {
        self.session.push_reasoning(s);
      } else {
        self.session.push_text(s);
      }
    }
    self.flush(false);
  }

  fn write_tag_aware(&mut self, s: &str, mut reasoning: bool) {
    let mut rest = s;
    loop {
      let open = rest.find("<think>");
      let close = rest.find("</think>");
      let (pos, is_open) = match (open, close) {
        (None, None) => {
          self.emit(rest, if reasoning { REASONING } else { TEXT });
          return;
        }
        (Some(o), Some(c)) => {
          if o <= c {
            (o, true)
          } else {
            (c, false)
          }
        }
        (Some(o), None) => (o, true),
        (None, Some(c)) => (c, false),
      };
      let (before, tail) = rest.split_at(pos);
      if !before.is_empty() {
        self.emit(before, if reasoning { REASONING } else { TEXT });
      }
      reasoning = is_open;
      rest = tail
        .strip_prefix("<think>")
        .or_else(|| tail.strip_prefix("</think>"))
        .unwrap_or_default();
      if rest.is_empty() {
        self.emit("", if reasoning { REASONING } else { TEXT });
        return;
      }
    }
  }

  fn flush(&mut self, force: bool) {
    if !force && self.last_flush.elapsed() < FLUSH_INTERVAL {
      return;
    }
    self.last_flush = Instant::now();
    self.absorb_user_input();
    if let Err(e) = transcript::write_file(&self.path, self.session.render()) {
      warn!("stream write failed: {e}");
    }
  }

  /// The snapshot this writer holds was taken before the request went out, so anything the human
  /// saved into the file while the answer streams is missing from it. Carry that forward before
  /// rendering, or the next flush silently deletes the words they just typed.
  fn absorb_user_input(&mut self) {
    let Ok(on_disk) = transcript::load(&self.path) else {
      return;
    };
    if on_disk.input.trim().is_empty() {
      return;
    }
    if on_disk.input.trim() == self.session.input.trim() {
      return;
    }
    self.session.input = on_disk.input;
  }

  pub fn finish_section(&mut self) {
    self.session.close_section();
    self.flush(true);
  }

  pub fn into_session(self) -> Session {
    self.session
  }
}
