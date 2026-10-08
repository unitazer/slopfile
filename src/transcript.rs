use crate::session::{self, ParseReport, Session};
use std::path::Path;

pub fn read_file(path: &Path) -> Result<String, String> {
  std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// Write via a temp file in the same directory and rename. A plain `fs::write` truncates first, so
/// a concurrent reader — another sweep, a `sf question`, an editor — can catch the file empty or
/// half-written and then save that shorter version back, losing the tail for good.
pub fn write_file(path: &Path, content: String) -> Result<(), String> {
  let dir = path.parent().unwrap_or_else(|| Path::new("."));
  let stem = path
    .file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_else(|| "session".to_string());
  // Unique per write, not just per process: several tasks in the daemon can save the same file.
  static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  let tmp = dir.join(format!(".{stem}.{}.{seq}.tmp", std::process::id()));
  std::fs::write(&tmp, content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
  match std::fs::rename(&tmp, path) {
    Ok(()) => Ok(()),
    Err(e) => {
      let _ = std::fs::remove_file(&tmp);
      Err(format!("rename {} to {}: {e}", tmp.display(), path.display()))
    }
  }
}

pub fn save(path: &Path, session: &Session) -> Result<(), String> {
  write_file(path, session.render())
}

/// Parse a transcript we already hold in memory, applying the same refusal as `load`.
pub fn load_str(text: &str) -> Session {
  transcript_load_checked(text).expect("re-parsing our own render must stay clean")
}

fn transcript_load_checked(text: &str) -> Result<Session, String> {
  let (session, report) = session::parse(text);
  if !report.is_clean() {
    return Err(report.problems().join("; "));
  }
  Ok(session)
}

/// Parse and refuse anything the model would have to fix by hand, with one canonical message.
pub fn load(path: &Path) -> Result<Session, String> {
  let text = read_file(path)?;
  transcript_load_checked(&text).map_err(|why| format!("failed to parse session file `{}`: {why}", path.display()))
}

pub fn load_lenient(path: &Path) -> Result<(Session, ParseReport), String> {
  let text = read_file(path)?;
  Ok(session::parse(&text))
}
