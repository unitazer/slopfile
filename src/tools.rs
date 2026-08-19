//! The 7 file tools (read, write, edit, bash, grep, find, ls) plus the special
//! `question` tool: OpenAI-style tool definitions plus the executor that the
//! agent loop calls with raw JSON arguments.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};

use crate::types::{FunctionDefinition, ToolDefinition, ToolResult};

/// Anything above this many characters in bash output gets truncated with a
/// "… [truncated N chars]" marker.
const BASH_OUTPUT_CAP: usize = 30_000;
const GREP_MATCH_CAP: usize = 500;
const FIND_FILE_CAP: usize = 2_000;

/// All tool definitions in the order the model sees (question is handled by
/// the agent loop, but the model must know it exists).
pub fn definitions() -> Vec<ToolDefinition> {
  vec![
    tool_def(
      "read",
      "Read a file as numbered lines, optionally skipping ahead and capping how many lines are shown. Paths are resolved relative to the workdir and must stay inside it.",
      json!({
          "type": "object",
          "properties": {
              "path": { "type": "string", "description": "File to read, relative to the workdir" },
              "offset": { "type": "integer", "description": "Skip this many lines", "default": 0 },
              "limit": { "type": "integer", "description": "Show at most this many lines", "default": 200 }
          },
          "required": ["path"]
      }),
    ),
    tool_def(
      "write",
      "Create or overwrite a file. Parent directories are created as needed. Paths are resolved relative to the workdir and must stay inside it.",
      json!({
          "type": "object",
          "properties": {
              "path": { "type": "string", "description": "File to write, relative to the workdir" },
              "content": { "type": "string", "description": "Full new contents of the file" }
          },
          "required": ["path", "content"]
      }),
    ),
    tool_def(
      "edit",
      "Replace the first occurrence of a text fragment in a file. Errors (with a line window of the file) if the fragment is not found.",
      json!({
          "type": "object",
          "properties": {
              "path": { "type": "string", "description": "File to edit, relative to the workdir" },
              "old": { "type": "string", "description": "Exact text to replace" },
              "new": { "type": "string", "description": "Replacement text" }
          },
          "required": ["path", "old", "new"]
      }),
    ),
    tool_def(
      "bash",
      "Run a shell command via `sh -c` in the workdir (or a subdirectory of it). Killed after the configured timeout; output is truncated to 30k chars.",
      json!({
          "type": "object",
          "properties": {
              "command": { "type": "string", "description": "Shell command line to run" },
              "cwd": { "type": "string", "description": "Working directory relative to the workdir; defaults to the workdir root" }
          },
          "required": ["command"]
      }),
    ),
    tool_def(
      "grep",
      "Recursively search files for a plain (non-regex) substring, printing `path:line_no: line`. Hidden directories (.git, etc.) and `target/` are skipped.",
      json!({
          "type": "object",
          "properties": {
              "pattern": { "type": "string", "description": "Plain substring to search for" },
              "path": { "type": "string", "description": "Directory to search, relative to the workdir", "default": "." },
              "glob": { "type": ["string", "null"], "description": "Unused optional filename filter" }
          },
          "required": ["pattern"]
      }),
    ),
    tool_def(
      "find",
      "Recursively list files whose name matches a suffix filter like `*.rs` (`*` alone lists everything). Hidden directories and `target/` are skipped.",
      json!({
          "type": "object",
          "properties": {
              "pattern": { "type": "string", "description": "Suffix filter, e.g. `*.rs`, or `*` for everything" },
              "path": { "type": "string", "description": "Directory to search, relative to the workdir", "default": "." }
          },
          "required": ["pattern"]
      }),
    ),
    tool_def(
      "ls",
      "List a directory as `name  <dir>` / `name  <size-bytes>` lines.",
      json!({
          "type": "object",
          "properties": {
              "path": { "type": "string", "description": "Directory to list, relative to the workdir", "default": "." }
          },
          "required": []
      }),
    ),
    tool_def(
      "question",
      "Ask the user multiple questions. Blocks until the user answers; not executed directly by the tool runner.",
      json!({
          "type": "object",
          "properties": {
              "desc": { "type": "string", "description": "Context for the questions" },
              "data": {
                  "type": "array",
                  "description": "Questions to ask",
                  "items": {
                      "type": "object",
                      "properties": {
                          "question": { "type": "string" },
                          "answers": { "type": "array", "items": { "type": "string" } },
                          "default": { "type": "string" }
                      },
                      "required": ["question", "answers"]
                  }
              }
          },
          "required": ["desc", "data"]
      }),
    ),
  ]
}

fn tool_def(name: &str, description: &str, parameters: Value) -> ToolDefinition {
  ToolDefinition {
    type_: "function".to_string(),
    function: FunctionDefinition {
      name: name.to_string(),
      description: Some(description.to_string()),
      parameters,
    },
  }
}

/// Execute one tool call. `arguments_json` is the raw JSON arguments object
/// from the model. Paths resolve relative to `workdir` and can never escape
/// it. `timeout_ms` bounds bash invocations.
pub async fn run(
  name: &str,
  arguments_json: &str,
  workdir: PathBuf,
  timeout_ms: u64,
) -> ToolResult {
  let args: Value = match serde_json::from_str(arguments_json) {
    Ok(v) => v,
    Err(e) => return ToolResult::err(format!("bad arguments json: {e}")),
  };
  match name {
    "read" => read(&workdir, &args),
    "write" => write(&workdir, &args),
    "edit" => edit(&workdir, &args),
    "bash" => bash(&workdir, &args, timeout_ms).await,
    "grep" => grep(&workdir, &args),
    "find" => find(&workdir, &args),
    "ls" => ls(&workdir, &args),
    "question" => ToolResult::err("question tool is handled by the agent loop, not executed here"),
    other => ToolResult::err(format!("unknown tool: {other}")),
  }
}

// ── Individual tools ────────────────────────────────────────────────────

fn read(workdir: &Path, args: &Value) -> ToolResult {
  let Some(path_str) = args.get("path").and_then(Value::as_str) else {
    return ToolResult::err("read: missing string argument \"path\"");
  };
  let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
  let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
  let path = match resolve_in(workdir, path_str) {
    Ok(p) => p,
    Err(e) => return ToolResult::err(e),
  };
  let content = match std::fs::read_to_string(&path) {
    Ok(c) => c,
    Err(e) => return ToolResult::err(format!("read: cannot read {path_str}: {e}")),
  };
  // Refuse to expose session transcripts / daemon-owned files. Any slopfile
  // runtime artifact (transcript, status line) carries this marker.
  if content.contains("<!-- slopfile") {
    return ToolResult::err("read: refusing to read a slopfile transcript or daemon-owned file");
  }
  let mut lines: Vec<&str> = content.split('\n').collect();
  if lines.last().is_some_and(|l| l.is_empty()) {
    lines.pop();
  }
  let mut out = String::new();
  for (i, line) in lines.iter().enumerate().skip(offset).take(limit) {
    out.push_str(&format!("{}: {line}\n", i + 1));
  }
  ToolResult::ok(out)
}

fn write(workdir: &Path, args: &Value) -> ToolResult {
  let Some(path_str) = args.get("path").and_then(Value::as_str) else {
    return ToolResult::err("write: missing string argument \"path\"");
  };
  let Some(content) = args.get("content").and_then(Value::as_str) else {
    return ToolResult::err("write: missing string argument \"content\"");
  };
  let path = match resolve_in(workdir, path_str) {
    Ok(p) => p,
    Err(e) => return ToolResult::err(e),
  };
  if let Some(parent) = path.parent()
    && let Err(e) = std::fs::create_dir_all(parent)
  {
    return ToolResult::err(format!("write: cannot create {path_str}: {e}"));
  }
  if let Err(e) = std::fs::write(&path, content) {
    return ToolResult::err(format!("write: cannot write {path_str}: {e}"));
  }
  ToolResult::ok(format!("wrote {} bytes", content.len()))
}

fn edit(workdir: &Path, args: &Value) -> ToolResult {
  let Some(path_str) = args.get("path").and_then(Value::as_str) else {
    return ToolResult::err("edit: missing string argument \"path\"");
  };
  let Some(old) = args.get("old").and_then(Value::as_str) else {
    return ToolResult::err("edit: missing string argument \"old\"");
  };
  let Some(new) = args.get("new").and_then(Value::as_str) else {
    return ToolResult::err("edit: missing string argument \"new\"");
  };
  if old.is_empty() {
    return ToolResult::err("edit: old text must not be empty");
  }
  let path = match resolve_in(workdir, path_str) {
    Ok(p) => p,
    Err(e) => return ToolResult::err(e),
  };
  let content = match std::fs::read_to_string(&path) {
    Ok(c) => c,
    Err(e) => return ToolResult::err(format!("edit: cannot read {path_str}: {e}")),
  };
  let Some(idx) = content.find(old) else {
    let window: Vec<&str> = content.lines().take(20).collect();
    return ToolResult::err(format!(
      "edit: old text not found in {path_str}\n--- first {} of {} lines ---\n{}",
      window.len(),
      content.lines().count(),
      window.join("\n")
    ));
  };
  let mut updated = content;
  updated.replace_range(idx..idx + old.len(), new);
  if let Err(e) = std::fs::write(&path, updated) {
    return ToolResult::err(format!("edit: cannot write {path_str}: {e}"));
  }
  ToolResult::ok(format!("edited {path_str}"))
}

async fn bash(workdir: &Path, args: &Value, timeout_ms: u64) -> ToolResult {
  let Some(command) = args.get("command").and_then(Value::as_str) else {
    return ToolResult::err("bash: missing string argument \"command\"");
  };
  let cwd_arg = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
  let cwd = match resolve_in(workdir, cwd_arg) {
    Ok(p) => p,
    Err(e) => return ToolResult::err(e),
  };
  let mut child = match tokio::process::Command::new("sh")
    .arg("-c")
    .arg(command)
    .current_dir(&cwd)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .process_group(0)
    .spawn()
  {
    Ok(c) => c,
    Err(e) => return ToolResult::err(format!("bash: failed to spawn sh: {e}")),
  };
  let stdout = child.stdout.take().expect("stdout is piped");
  let stderr = child.stderr.take().expect("stderr is piped");
  let outcome = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), async {
    let (out, err) = tokio::join!(read_into(stdout), read_into(stderr));
    let status = child
      .wait()
      .await
      .map_err(|e| format!("bash: wait failed: {e}"))?;
    Ok::<_, String>((out, err, status))
  })
  .await;
  match outcome {
    Err(_) => {
      // Group id == child pid (process_group(0)); capture it BEFORE the kill
      // — tokio reaps the child and `id()` returns None after `kill()`.
      let group_id = child.id();
      let _ = child.kill().await;
      // `sh -c 'sleep 1000 &'` backgrounds children into the same process
      // group; kill the whole group (negative pid), best effort.
      if let Some(id) = group_id {
        let _ = std::process::Command::new("kill")
          .args(["-9", &format!("-{id}")])
          .stderr(Stdio::null())
          .status();
      }
      ToolResult::err(format!(
        "bash: timeout after {timeout_ms}ms, process killed: {command}"
      ))
    }
    Ok(Err(e)) => ToolResult::err(e),
    Ok(Ok((stdout_s, stderr_s, status))) => {
      let mut combined = stdout_s;
      if !stderr_s.is_empty() {
        if !combined.is_empty() {
          combined.push('\n');
        }
        combined.push_str(&stderr_s);
      }
      let combined = truncate(&combined);
      if status.success() {
        ToolResult::ok(combined)
      } else {
        let code = status
          .code()
          .map(|c| c.to_string())
          .unwrap_or_else(|| "signal".to_string());
        ToolResult::err(format!("bash: exit {code}:\n{combined}"))
      }
    }
  }
}

fn grep(workdir: &Path, args: &Value) -> ToolResult {
  let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
    return ToolResult::err("grep: missing string argument \"pattern\"");
  };
  if pattern.is_empty() {
    return ToolResult::err("grep: empty pattern matches everything; refused");
  }
  let path_arg = args.get("path").and_then(Value::as_str).unwrap_or(".");
  let root = match resolve_in(workdir, path_arg) {
    Ok(p) => p,
    Err(e) => return ToolResult::err(e),
  };
  let files = match walk_files(workdir, &root) {
    Ok(f) => f,
    Err(e) => return ToolResult::err(e),
  };
  let mut out = String::new();
  let mut hits = 0usize;
  'walk: for (rel, abs) in &files {
    let Ok(content) = std::fs::read_to_string(abs) else {
      continue;
    };
    for (idx, line) in content.lines().enumerate() {
      if line.contains(pattern) {
        hits += 1;
        if hits > GREP_MATCH_CAP {
          out.push_str(&format!(
            "… [truncated: more than {GREP_MATCH_CAP} matching lines]\n"
          ));
          break 'walk;
        }
        out.push_str(&format!("{}:{}: {line}\n", rel.display(), idx + 1));
      }
    }
  }
  ToolResult::ok(out)
}

fn find(workdir: &Path, args: &Value) -> ToolResult {
  let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
    return ToolResult::err("find: missing string argument \"pattern\"");
  };
  if pattern.is_empty() {
    return ToolResult::err("find: empty pattern matches everything; refused");
  }
  let path_arg = args.get("path").and_then(Value::as_str).unwrap_or(".");
  let root = match resolve_in(workdir, path_arg) {
    Ok(p) => p,
    Err(e) => return ToolResult::err(e),
  };
  let files = match walk_files(workdir, &root) {
    Ok(f) => f,
    Err(e) => return ToolResult::err(e),
  };
  let mut out = String::new();
  let mut hits = 0usize;
  for (rel, _abs) in &files {
    let name = rel
      .file_name()
      .map(|n| n.to_string_lossy().into_owned())
      .unwrap_or_default();
    if !matches_find(&name, pattern) {
      continue;
    }
    hits += 1;
    if hits > FIND_FILE_CAP {
      out.push_str(&format!("… [truncated: more than {FIND_FILE_CAP} files]\n"));
      break;
    }
    out.push_str(&format!("{}\n", rel.display()));
  }
  ToolResult::ok(out)
}

fn ls(workdir: &Path, args: &Value) -> ToolResult {
  let path_arg = args.get("path").and_then(Value::as_str).unwrap_or(".");
  let root = match resolve_in(workdir, path_arg) {
    Ok(p) => p,
    Err(e) => return ToolResult::err(e),
  };
  let meta = match std::fs::metadata(&root) {
    Ok(m) => m,
    Err(e) => return ToolResult::err(format!("ls: cannot access {path_arg}: {e}")),
  };
  if meta.is_file() {
    let name = root
      .file_name()
      .map(|n| n.to_string_lossy().into_owned())
      .unwrap_or_else(|| path_arg.to_string());
    return ToolResult::ok(format!("{name}  {}", meta.len()));
  }
  let mut entries: Vec<std::fs::DirEntry> = Vec::new();
  let rd = match std::fs::read_dir(&root) {
    Ok(r) => r,
    Err(e) => return ToolResult::err(format!("ls: cannot read {path_arg}: {e}")),
  };
  for entry in rd {
    match entry {
      Ok(e) => entries.push(e),
      Err(e) => return ToolResult::err(format!("ls: read_dir {path_arg}: {e}")),
    }
  }
  entries.sort_by_key(|e| e.file_name());
  let mut out = String::new();
  for entry in entries {
    let name = entry.file_name().to_string_lossy().into_owned();
    let ft = match entry.file_type() {
      Ok(t) => t,
      Err(e) => return ToolResult::err(format!("ls: file_type {name}: {e}")),
    };
    if ft.is_dir() {
      out.push_str(&format!("{name}  <dir>\n"));
    } else if ft.is_file() {
      let size = std::fs::metadata(entry.path())
        .map(|m| m.len())
        .unwrap_or(0);
      out.push_str(&format!("{name}  {size}\n"));
    }
  }
  ToolResult::ok(out)
}

// ── Path safety ─────────────────────────────────────────────────────────

/// Resolve `p` (workdir-relative or absolute) to a canonical path, rejecting
/// anything that ends up outside `workdir`. For paths that do not exist yet
/// (e.g. a write target), the deepest existing ancestor is canonicalized and
/// the missing tail is appended. Never panics; errors carry a readable
/// message.
fn resolve_in(workdir: &Path, p: &str) -> Result<PathBuf, String> {
  let base = std::fs::canonicalize(workdir).map_err(|e| format!("workdir does not exist: {e}"))?;
  let target = if Path::new(p).is_absolute() {
    PathBuf::from(p)
  } else {
    base.join(p)
  };
  let mut missing: Vec<std::ffi::OsString> = Vec::new();
  let mut cur = target.as_path();
  loop {
    match std::fs::canonicalize(cur) {
      Ok(existing) => {
        let mut resolved = existing;
        for part in missing.iter().rev() {
          resolved.push(part);
        }
        if !resolved.starts_with(&base) {
          return Err(format!("path escapes workdir: {p}"));
        }
        return Ok(resolved);
      }
      Err(_) => {
        let Some(name) = cur.file_name() else {
          return Err(format!("cannot resolve path: {p}"));
        };
        missing.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
          return Err(format!("cannot resolve path: {p}"));
        };
        cur = parent;
      }
    }
  }
}

// ── Filesystem walking ──────────────────────────────────────────────────

/// Recursively list every regular file under `root`, returning `(rel, abs)`
/// pairs where `rel` is the path relative to `workdir` (for display). Hidden
/// directories (`.git`, …) and `target/` are skipped. Entries are visited in
/// sorted order per directory, breadth-first, so output is deterministic.
fn walk_files(workdir: &Path, root: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
  let mut files = Vec::new();
  if root.is_file() {
    let rel = root
      .strip_prefix(workdir)
      .map(Path::to_path_buf)
      .unwrap_or_else(|_| root.to_path_buf());
    files.push((rel, root.to_path_buf()));
    return Ok(files);
  }
  if !root.is_dir() {
    return Err(format!("not a directory: {}", root.display()));
  }
  let mut queue = VecDeque::from([root.to_path_buf()]);
  while let Some(dir) = queue.pop_front() {
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    let mut entries: Vec<std::fs::DirEntry> = Vec::new();
    for entry in rd {
      entries.push(entry.map_err(|e| format!("read_dir {}: {e}", dir.display()))?);
    }
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
      let path = entry.path();
      let ft = entry
        .file_type()
        .map_err(|e| format!("file_type {}: {e}", path.display()))?;
      if ft.is_dir() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" {
          continue;
        }
        queue.push_back(path);
      } else if ft.is_file() {
        let rel = path
          .strip_prefix(workdir)
          .map(Path::to_path_buf)
          .unwrap_or_else(|_| path.clone());
        files.push((rel, path));
      }
    }
  }
  Ok(files)
}

fn matches_find(name: &str, pattern: &str) -> bool {
  if pattern == "*" {
    true
  } else if let Some(suffix) = pattern.strip_prefix('*') {
    name.ends_with(suffix)
  } else {
    name == pattern
  }
}

fn truncate(s: &str) -> String {
  if s.len() <= BASH_OUTPUT_CAP {
    return s.to_string();
  }
  let cutoff = s
    .char_indices()
    .nth(BASH_OUTPUT_CAP)
    .map(|(i, _)| i)
    .unwrap_or(s.len());
  let mut out = s[..cutoff].to_string();
  let dropped = s[cutoff..].chars().count();
  out.push_str(&format!("\n… [truncated {dropped} chars]"));
  out
}

/// Read an async pipe to the end, lossy-converted to a String.
async fn read_into(mut pipe: impl tokio::io::AsyncRead + Unpin) -> String {
  use tokio::io::AsyncReadExt;
  let mut buf = Vec::new();
  let _ = pipe.read_to_end(&mut buf).await;
  String::from_utf8_lossy(&buf).into_owned()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use std::fs;

  use serde_json::{Value, json};

  use super::*;

  /// Fresh empty scratch dir under the system temp dir, unique per tag.
  /// No tempfile dependency: clean up by removing any previous run's dir.
  fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("slopfile-tools-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
  }

  async fn t(name: &str, args: Value, dir: PathBuf) -> ToolResult {
    run(name, &args.to_string(), dir, 30_000).await
  }

  #[tokio::test]
  async fn read_rejects_transcript_files() {
    let wd = test_dir("read-transcript");
    let content = "### You\nhi\n### Assistant\nhi\n<!-- slopfile:live {\"phase\":\"idle\"} -->";
    let _ = t("write", json!({"path": "transcript.md", "content": content}), wd.clone()).await;
    let w2 = t("write", json!({"path": "plain.md", "content": "just text\n"}), wd.clone()).await;
    assert!(w2.ok, "write plain failed: {}", w2.output);

    let r = t("read", json!({"path": "transcript.md"}), wd.clone()).await;
    assert!(!r.ok, "transcript read should be refused");
    assert!(r.output.contains("refusing"), "unexpected msg: {}", r.output);

    let ok = t("read", json!({"path": "plain.md"}), wd.clone()).await;
    assert!(ok.ok, "plain read should succeed: {}", ok.output);
  }

  #[tokio::test]
  async fn read_write_edit_round_trip() {
    let wd = test_dir("roundtrip");
    let w = t(
      "write",
      json!({"path": "f.txt", "content": "alpha\nbeta\ngamma\n"}),
      wd.clone(),
    )
    .await;
    assert!(w.ok, "write failed: {}", w.output);
    assert_eq!(w.output, "wrote 17 bytes");

    let r = t("read", json!({"path": "f.txt"}), wd.clone()).await;
    assert!(r.ok, "read failed: {}", r.output);
    assert_eq!(r.output, "1: alpha\n2: beta\n3: gamma\n");

    let w2 = t(
      "read",
      json!({"path": "f.txt", "offset": 1, "limit": 2}),
      wd.clone(),
    )
    .await;
    assert!(w2.ok, "read window failed: {}", w2.output);
    assert_eq!(w2.output, "2: beta\n3: gamma\n");

    let e = t(
      "edit",
      json!({"path": "f.txt", "old": "beta", "new": "BETA"}),
      wd.clone(),
    )
    .await;
    assert!(e.ok, "edit failed: {}", e.output);

    let r2 = t("read", json!({"path": "f.txt"}), wd.clone()).await;
    assert!(r2.ok, "read after edit failed: {}", r2.output);
    assert_eq!(r2.output, "1: alpha\n2: BETA\n3: gamma\n");

    let edit_first = t(
      "edit",
      json!({"path": "f.txt", "old": "a", "new": "X"}),
      wd.clone(),
    )
    .await;
    assert!(
      edit_first.ok,
      "edit first occurrence failed: {}",
      edit_first.output
    );
    let r3 = t("read", json!({"path": "f.txt"}), wd.clone()).await;
    assert_eq!(r3.output, "1: Xlpha\n2: BETA\n3: gamma\n");

    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn edit_not_found_lists_file_window() {
    let wd = test_dir("edit-miss");
    let w = t(
      "write",
      json!({"path": "f.txt", "content": "one\ntwo\nthree\n"}),
      wd.clone(),
    )
    .await;
    assert!(w.ok, "write failed: {}", w.output);
    let e = t(
      "edit",
      json!({"path": "f.txt", "old": "zzz", "new": "q"}),
      wd.clone(),
    )
    .await;
    assert!(!e.ok);
    assert!(e.output.contains("not found"), "unexpected: {}", e.output);
    assert!(
      e.output.contains("first 3 of 3 lines") && e.output.contains("one\ntwo\nthree"),
      "window missing: {}",
      e.output
    );
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn write_creates_nested_parent_dirs() {
    let wd = test_dir("nested");
    let w = t(
      "write",
      json!({"path": "deep/nested/file.txt", "content": "hi"}),
      wd.clone(),
    )
    .await;
    assert!(w.ok, "write failed: {}", w.output);
    assert!(wd.join("deep/nested/file.txt").is_file());
    let r = t("read", json!({"path": "deep/nested/file.txt"}), wd.clone()).await;
    assert!(r.ok);
    assert_eq!(r.output, "1: hi\n");
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn read_rejects_paths_outside_workdir() {
    let wd = test_dir("read-escape");
    fs::write(wd.join("ok.txt"), "fine").expect("seed file");
    for bad in ["../etc/passwd", "/etc/passwd", "../../etc/passwd"] {
      let r = t("read", json!({"path": bad}), wd.clone()).await;
      assert!(!r.ok, "{bad} was not rejected: {}", r.output);
      assert!(
        r.output.contains("escape"),
        "{bad} rejection unclear: {}",
        r.output
      );
    }
    let ok = t("read", json!({"path": "ok.txt"}), wd.clone()).await;
    assert!(ok.ok);
    let miss = t("read", json!({"path": "missing.txt"}), wd.clone()).await;
    assert!(!miss.ok, "missing file should error");
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn write_rejects_paths_outside_workdir_and_bad_args() {
    let wd = test_dir("write-escape");
    for bad in ["../evil.txt", "/tmp/slopfile-evil-{}.txt", "../../evil.txt"] {
      let w = t("write", json!({"path": bad, "content": "x"}), wd.clone()).await;
      assert!(!w.ok, "{bad} was not rejected: {}", w.output);
      assert!(
        w.output.contains("escape"),
        "{bad} rejection unclear: {}",
        w.output
      );
    }
    assert!(!wd.parent().unwrap().join("evil.txt").exists());
    let no_args = t("write", json!({"path": "x.txt"}), wd.clone()).await;
    assert!(!no_args.ok);
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn ls_lists_names_and_sizes() {
    let wd = test_dir("ls");
    fs::write(wd.join("a.txt"), "hello").expect("seed file");
    fs::create_dir(wd.join("sub")).expect("seed dir");
    let r = t("ls", json!({"path": "."}), wd.clone()).await;
    assert!(r.ok, "ls failed: {}", r.output);
    assert!(
      r.output.contains("a.txt  5"),
      "missing file line: {}",
      r.output
    );
    assert!(
      r.output.contains("sub  <dir>"),
      "missing dir line: {}",
      r.output
    );
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn find_filters_by_suffix() {
    let wd = test_dir("find");
    fs::write(wd.join("a.txt"), "").expect("seed file");
    fs::write(wd.join("b.rs"), "").expect("seed file");
    fs::create_dir(wd.join("sub")).expect("seed dir");
    fs::write(wd.join("sub/c.rs"), "").expect("seed file");
    fs::write(wd.join("sub/d.log"), "").expect("seed file");
    let r = t("find", json!({"pattern": "*.rs"}), wd.clone()).await;
    assert!(r.ok, "find failed: {}", r.output);
    assert!(r.output.contains("b.rs"), "missing b.rs: {}", r.output);
    assert!(
      r.output.contains("sub/c.rs"),
      "missing sub/c.rs: {}",
      r.output
    );
    assert!(
      !r.output.contains("a.txt"),
      "suffix filter leaked: {}",
      r.output
    );
    assert!(
      !r.output.contains("d.log"),
      "suffix filter leaked: {}",
      r.output
    );
    let all = t("find", json!({"pattern": "*"}), wd.clone()).await;
    assert!(all.ok);
    assert_eq!(
      all.output.lines().count(),
      4,
      "expected 4 files: {}",
      all.output
    );
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn grep_lists_line_matches_and_skips_hidden() {
    let wd = test_dir("grep");
    fs::write(wd.join("a.txt"), "hello world\nfoo bar\n").expect("seed file");
    fs::create_dir(wd.join("sub")).expect("seed dir");
    fs::write(wd.join("sub/b.txt"), "world again\n").expect("seed file");
    fs::create_dir(wd.join("target")).expect("seed target");
    fs::write(wd.join("target/secret.txt"), "hello world\n").expect("seed file");
    fs::create_dir(wd.join(".git")).expect("seed .git");
    fs::write(wd.join(".git/hidden.txt"), "hello world\n").expect("seed file");
    let r = t("grep", json!({"pattern": "world"}), wd.clone()).await;
    assert!(r.ok, "grep failed: {}", r.output);
    assert!(r.output.contains("a.txt:1: hello world"), "{}", r.output);
    assert!(
      r.output.contains("sub/b.txt:1: world again"),
      "{}",
      r.output
    );
    assert!(
      !r.output.contains("target"),
      "target not skipped: {}",
      r.output
    );
    assert!(!r.output.contains(".git"), ".git not skipped: {}", r.output);
    let sub = t(
      "grep",
      json!({"pattern": "world", "path": "sub"}),
      wd.clone(),
    )
    .await;
    assert!(sub.ok);
    assert!(sub.output.contains("sub/b.txt:1: world again"));
    assert!(!sub.output.contains("a.txt"));
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn bash_echo_succeeds() {
    let wd = test_dir("bash-echo");
    let r = t("bash", json!({"command": "echo hello"}), wd.clone()).await;
    assert!(r.ok, "echo failed: {}", r.output);
    assert!(r.output.contains("hello"), "unexpected: {}", r.output);
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn bash_nonzero_exit_errors_with_output() {
    let wd = test_dir("bash-exit");
    let r = t("bash", json!({"command": "echo boom; exit 1"}), wd.clone()).await;
    assert!(!r.ok, "exit 1 should error");
    assert!(r.output.contains("exit"), "missing exit code: {}", r.output);
    assert!(r.output.contains("boom"), "missing output: {}", r.output);
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn bash_timeout_kills_the_child() {
    let wd = test_dir("bash-timeout");
    let r = run("bash", "{\"command\":\"sleep 5\"}", wd.clone(), 200).await;
    assert!(!r.ok, "sleep 5 with 200ms timeout should have timed out");
    assert!(r.output.contains("timeout"), "unexpected: {}", r.output);
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn bash_timeout_kills_backgrounded_processes() {
    let wd = test_dir("bash-bgkill");
    let pid_file = wd.join("bg.pid");
    let cmd = format!("sleep 60 & echo $! > {}", pid_file.display());
    let r = run("bash", &json!({ "command": cmd }).to_string(), wd.clone(), 200).await;
    assert!(!r.ok, "should have timed out");
    assert!(r.output.contains("timeout"), "unexpected: {}", r.output);
    let bg_pid: i32 = fs::read_to_string(&pid_file)
      .expect("background pid file")
      .trim()
      .parse()
      .expect("pid is a number");
    // The whole process group (incl. backgrounded grandchildren) must be
    // gone: `kill -0` fails once the process no longer exists.
    for _ in 0..10 {
      let alive = std::process::Command::new("kill")
        .args(["-0", &bg_pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("kill -0");
      if !alive.success() {
        return;
      }
      tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("backgrounded process {bg_pid} survived the timeout kill");
  }

  #[tokio::test]
  async fn question_errors_by_design() {
    let wd = test_dir("question");
    let r = t(
      "question",
      json!({"desc": "d", "data": [{"question": "red or blue", "answers": ["red", "blue"]}]}),
      wd.clone(),
    )
    .await;
    assert!(!r.ok, "question must not run");
    assert!(
      r.output.contains("agent loop"),
      "unexpected message: {}",
      r.output
    );
    let _ = fs::remove_dir_all(&wd);
  }

  #[test]
  fn definitions_match_contract_shape() {
    let defs = definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    assert_eq!(
      names,
      [
        "read", "write", "edit", "bash", "grep", "find", "ls", "question"
      ]
    );
    for d in &defs {
      assert_eq!(d.type_, "function", "bad type_ for {}", d.function.name);
      assert_eq!(
        d.function.parameters["type"].as_str(),
        Some("object"),
        "bad parameters for {}",
        d.function.name
      );
    }
    let q = defs.iter().find(|d| d.function.name == "question").unwrap();
    let p = &q.function.parameters;
    assert_eq!(p["required"], json!(["desc", "data"]));
    assert_eq!(p["properties"]["desc"]["type"], json!("string"));
    let items = &p["properties"]["data"]["items"];
    assert_eq!(items["required"], json!(["question", "answers"]));
    assert_eq!(items["properties"]["question"]["type"], json!("string"));
    assert_eq!(items["properties"]["answers"]["type"], json!("array"));
    assert_eq!(items["properties"]["default"]["type"], json!("string"));
    assert!(
      p["properties"]["data"]["items"]["properties"]["default"]
        .as_object()
        .is_some()
    );
  }
}
