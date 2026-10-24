use std::collections::VecDeque;
use std::fs::{create_dir_all, read_to_string};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::process::Command;

use crate::types::{FunctionDefinition, SessionConfig, ToolDefinition, ToolResult};

const BASH_OUTPUT_CAP: usize = 30_000;
const GREP_MATCH_CAP: usize = 500;
const GREP_LINE_CAP: usize = 2_000;
const FIND_FILE_CAP: usize = 2_000;
const WALK_FILE_CAP: usize = 50_000;

pub fn definitions() -> Vec<ToolDefinition> {
  vec![
    tool_def(
      "read",
      "Read a file as numbered lines, optionally skipping ahead and capping how many lines are shown. Paths are resolved relative to the workdir and must stay inside it, except for your own context file, which is named in the system prompt and always readable.",
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
      "Create or overwrite a file. Parent directories are created as needed. Paths are resolved relative to the workdir and must stay inside it, except for your own context file, which you may rewrite to manage your context.",
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
      "Replace the first occurrence of a text fragment in a file. Errors (with a line window of the file) if the fragment is not found. Your own context file may be edited this way.",
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
      "subagent",
      "Spawn a subagent that works on a task in its own fresh context. It has the same file and shell tools as you but no `question` tool, and it cannot see this conversation. Give it a self-contained task with all needed context. Its full final report is delivered back to you automatically when it finishes.",
      json!({
          "type": "object",
          "properties": {
              "name": { "type": "string", "description": "Short identifier for the subagent, e.g. 'test-runner'" },
              "task": { "type": "string", "description": "Complete, self-contained description of what the subagent should do" }
          },
          "required": ["name", "task"]
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

pub async fn run(
  name: &str,
  arguments_json: &str,
  workdir: PathBuf,
  timeout_ms: u64,
  extra_read_roots: &[PathBuf],
  context_file: Option<&Path>,
) -> ToolResult {
  let args: Value = match serde_json::from_str(arguments_json) {
    Ok(v) => v,
    Err(e) => return ToolResult::err(format!("bad arguments json: {e}")),
  };
  match name {
    "read" => read(&workdir, extra_read_roots, &args, context_file),
    "write" => write(&workdir, &args, context_file),
    "edit" => edit(&workdir, &args, context_file),
    "bash" => bash(&workdir, &args, timeout_ms).await,
    "grep" => grep(&workdir, &args),
    "find" => find(&workdir, &args),
    "ls" => ls(&workdir, &args),
    "question" => ToolResult::err("question tool is handled by the agent loop, not executed here"),
    other => ToolResult::err(format!("unknown tool: {other}")),
  }
}

/// The tool definitions this session is actually allowed to call. The daemon advertises exactly
/// this list, so the model is never told about a tool that would come back disabled.
pub fn advertised(config: &SessionConfig) -> Vec<ToolDefinition> {
  definitions()
    .into_iter()
    .filter(|d| config.allows_tool(&d.function.name))
    .collect()
}

fn targets_context_file(workdir: &Path, asked: &str, context_file: Option<&Path>) -> bool {
  let Some(context_file) = context_file else {
    return false;
  };
  let candidate = if Path::new(asked).is_absolute() {
    PathBuf::from(asked)
  } else {
    workdir.join(asked)
  };
  match (
    std::fs::canonicalize(&candidate),
    std::fs::canonicalize(context_file),
  ) {
    (Ok(a), Ok(c)) => a == c,
    _ => candidate == context_file,
  }
}

fn resolve_target(
  workdir: &Path,
  asked: &str,
  context_file: Option<&Path>,
) -> Result<PathBuf, String> {
  if !targets_context_file(workdir, asked, context_file) {
    return resolve_in(workdir, asked);
  }
  let context_file = context_file.expect("checked by targets_context_file");
  std::fs::canonicalize(context_file)
    .map_err(|e| format!("cannot open the context file {asked}: {e}"))
}

fn str_arg<'a>(tool: &str, args: &'a Value, key: &str) -> Result<&'a str, ToolResult> {
  args
    .get(key)
    .and_then(Value::as_str)
    .ok_or_else(|| ToolResult::err(format!("{tool}: missing string argument \"{key}\"")))
}

macro_rules! arg {
  ($tool:expr, $args:expr, $key:expr) => {
    match str_arg($tool, $args, $key) {
      Ok(v) => v,
      Err(e) => return e,
    }
  };
}

macro_rules! path {
  ($workdir:expr, $s:expr) => {
    match resolve_in($workdir, $s) {
      Ok(p) => p,
      Err(e) => return ToolResult::err(e),
    }
  };
}

fn read(
  workdir: &Path,
  extra_roots: &[PathBuf],
  args: &Value,
  context_file: Option<&Path>,
) -> ToolResult {
  let path_str = arg!("read", args, "path");
  let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
  let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
  let is_context = targets_context_file(workdir, path_str, context_file);
  let path = if is_context {
    match resolve_target(workdir, path_str, context_file) {
      Ok(p) => p,
      Err(e) => return ToolResult::err(format!("read: {e}")),
    }
  } else if extra_roots.is_empty() {
    path!(workdir, path_str)
  } else {
    // The system prompt advertises `skills_dir` verbatim, so a relative extra root has to resolve
    // even when the path is perfectly valid but simply does not exist under the workdir.
    let in_workdir = resolve_in(workdir, path_str).ok();
    let elsewhere = extra_roots
      .iter()
      .find_map(|root| resolve_in(root, path_str).ok());
    match in_workdir.filter(|p| p.exists()).or_else(|| elsewhere) {
      Some(p) => p,
      None => return ToolResult::err(format!("read: path escapes workdir: {path_str}")),
    }
  };
  // Read only the window the model asked for: a 200 MB file with limit 200 still cost 200 MB
  // resident in the one process that hosts every session.
  let content = match read_window(&path, offset, limit) {
    Ok(c) => c,
    Err(e) => return ToolResult::err(format!("read: cannot read {path_str}: {e}")),
  };
  let mut lines: Vec<&str> = content.split('\n').collect();
  if lines.last().is_some_and(|l| l.is_empty()) {
    lines.pop();
  }
  let mut out = String::new();
  for (i, line) in lines.iter().enumerate() {
    out.push_str(&format!("{}: {line}\n", i + 1 + offset));
  }
  // The line numbers must stay absolute, and one call must not permanently inflate the context of
  // the whole session: it is re-sent in the body of every later request.
  if out.len() > BASH_OUTPUT_CAP {
    let cut = floor_char_boundary(&out, BASH_OUTPUT_CAP);
    out.truncate(cut);
    out.push_str(&format!(
      "\n… [truncated at {BASH_OUTPUT_CAP} chars; ask for a narrower range to see more]"
    ));
  }
  ToolResult::ok(out)
}

/// Read only `offset + limit` lines, streaming rather than slurping the whole file.
fn read_window(path: &Path, offset: usize, limit: usize) -> std::io::Result<String> {
  use std::io::{BufRead, BufReader};
  let file = std::fs::File::open(path)?;
  let mut reader = BufReader::with_capacity(64 * 1024, file);
  let mut out = String::new();
  let mut line = String::new();
  let mut index = 0usize;
  let mut emitted = 0usize;
  loop {
    line.clear();
    if reader.read_line(&mut line)? == 0 {
      break;
    }
    index += 1;
    if index > offset {
      out.push_str(&line);
      emitted += 1;
      if emitted >= limit || out.len() > BASH_OUTPUT_CAP {
        break;
      }
    }
  }
  Ok(out)
}

fn write(workdir: &Path, args: &Value, context_file: Option<&Path>) -> ToolResult {
  let path_str = arg!("write", args, "path");
  let content = arg!("write", args, "content");
  let is_context = targets_context_file(workdir, path_str, context_file);
  let path = if is_context {
    match resolve_target(workdir, path_str, context_file) {
      Ok(p) => p,
      Err(e) => return ToolResult::err(format!("write: {e}")),
    }
  } else {
    path!(workdir, path_str)
  };
  if is_context {
    if let Err(why) = crate::context::check_edit(content) {
      return ToolResult::err(format!(
        "write: {path_str} is this session's context file and that rewrite was refused: {why}"
      ));
    }
  }
  if let Some(parent) = path.parent()
    && let Err(e) = create_dir_all(parent)
  {
    return ToolResult::err(format!("write: cannot create {path_str}: {e}"));
  }
  if let Err(e) = std::fs::write(&path, content) {
    return ToolResult::err(format!("write: cannot write {path_str}: {e}"));
  }
  ToolResult::ok(format!("wrote {} bytes", content.len()))
}

fn edit(workdir: &Path, args: &Value, context_file: Option<&Path>) -> ToolResult {
  let path_str = arg!("edit", args, "path");
  let old = arg!("edit", args, "old");
  let new = arg!("edit", args, "new");
  if old.is_empty() {
    return ToolResult::err("edit: old text must not be empty");
  }
  let is_context = targets_context_file(workdir, path_str, context_file);
  let path = if is_context {
    match resolve_target(workdir, path_str, context_file) {
      Ok(p) => p,
      Err(e) => return ToolResult::err(format!("edit: {e}")),
    }
  } else {
    path!(workdir, path_str)
  };
  let content = match read_to_string(&path) {
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
  if is_context {
    if let Err(why) = crate::context::check_edit(&updated) {
      return ToolResult::err(format!(
        "edit: that change to {path_str} would break this session's context file, so it was refused: {why}"
      ));
    }
  }
  if let Err(e) = std::fs::write(&path, updated) {
    return ToolResult::err(format!("edit: cannot write {path_str}: {e}"));
  }
  ToolResult::ok(format!("edited {path_str}"))
}

async fn bash(workdir: &Path, args: &Value, timeout_ms: u64) -> ToolResult {
  let command = arg!("bash", args, "command");
  let cwd_arg = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
  let cwd = path!(workdir, cwd_arg);
  let mut child = match Command::new("sh")
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
  // Read both pipes into shared buffers *while* the command runs, so a chatty command cannot
  // deadlock on a full pipe. But never treat EOF as the stopping condition: a backgrounded job
  // inherits the pipe (`cd x && nohup foo > log & echo hi` backgrounds a subshell whose own stdout
  // is still ours), so waiting for EOF would hang until the timeout even though the command
  // finished immediately -- and the timeout kill would then destroy the job it had just launched.
  let out_buf = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
  let err_buf = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
  let out_task = tokio::spawn(drain_into(stdout, out_buf.clone()));
  let err_task = tokio::spawn(drain_into(stderr, err_buf.clone()));

  let waited =
    tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), child.wait()).await;
  let status = match waited {
    Err(_) => {
      out_task.abort();
      err_task.abort();
      let group_id = child.id();
      let _ = child.kill().await;
      if let Some(id) = group_id {
        let _ = std::process::Command::new("kill")
          .args(["-9", &format!("-{id}")])
          .stderr(Stdio::null())
          .status();
      }
      return ToolResult::err(format!(
        "bash: timeout after {timeout_ms}ms, process killed: {command}"
      ));
    }
    Ok(Err(e)) => {
      out_task.abort();
      err_task.abort();
      return ToolResult::err(format!("bash: wait failed: {e}"));
    }
    Ok(Ok(status)) => status,
  };

  // The command has exited, so anything it printed is already buffered; give the pipes a moment
  // to land the last of it, then stop caring about them.
  tokio::time::sleep(std::time::Duration::from_millis(120)).await;
  out_task.abort();
  err_task.abort();
  let out = String::from_utf8_lossy(&out_buf.lock().await).into_owned();
  let err = String::from_utf8_lossy(&err_buf.lock().await).into_owned();
  let truncated = out.len() > BASH_OUTPUT_CAP || err.len() > BASH_OUTPUT_CAP;
  let out = cap_string(out);
  let err = cap_string(err);
  let outcome = Some((out, err, status, truncated));
  match outcome {
    None => {
      let group_id = child.id();
      let _ = child.kill().await;
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
    Some((stdout_s, stderr_s, status, truncated)) => {
      let mut combined = stdout_s;
      if !stderr_s.is_empty() {
        if !combined.is_empty() {
          combined.push('\n');
        }
        combined.push_str(&stderr_s);
      }
      let combined = truncate(&combined);
      let real_code = status.code();
      let note = if truncated {
        format!(
          "\n[output truncated at {BASH_OUTPUT_CAP} chars; the command ran to completion and its \
           exit status is real, but re-run with output redirected to a file if you need the rest]"
        )
      } else {
        String::new()
      };
      match (status.success(), real_code, truncated) {
        // It exited cleanly, so it succeeded even though we only kept the first N chars.
        (true, _, true) => return ToolResult::ok(format!("{combined}{note}")),
        (true, _, false) => return ToolResult::ok(combined),
        // A real exit code is the command's own verdict, truncation or not: it failed.
        (false, Some(code), _) => {
          return ToolResult::err(format!("bash: exit {code}:\n{combined}{note}"));
        }
        // Unreachable in practice: we drain to EOF rather than closing the pipe at the cap, so
        // there is no SIGPIPE and a signal death is a genuine crash.
        (false, None, true) => {
          return ToolResult::err(format!(
            "bash: killed by signal after {BASH_OUTPUT_CAP} chars:\n{combined}{note}"
          ));
        }
        // A signal death we did not cause is a genuine crash.
        (false, None, false) => {
          return ToolResult::err(format!("bash: killed by signal:\n{combined}"));
        }
      }
    }
  }
}

fn grep(workdir: &Path, args: &Value) -> ToolResult {
  let pattern = arg!("grep", args, "pattern");
  if pattern.is_empty() {
    return ToolResult::err("grep: empty pattern matches everything; refused");
  }
  let path_arg = args.get("path").and_then(Value::as_str).unwrap_or(".");
  let root = path!(workdir, path_arg);
  let files = match walk_files(workdir, &root) {
    Ok(f) => f,
    Err(e) => return ToolResult::err(e),
  };
  let mut out = String::new();
  let mut hits = 0usize;
  'walk: for (rel, abs) in &files {
    let Ok(content) = read_to_string(abs) else {
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
        let shown = if line.chars().count() > GREP_LINE_CAP {
          let head: String = line.chars().take(GREP_LINE_CAP).collect();
          format!("{head}… [line truncated]")
        } else {
          line.to_string()
        };
        out.push_str(&format!("{}:{}: {shown}\n", rel.display(), idx + 1));
      }
    }
  }
  ToolResult::ok(truncate(&out))
}

fn find(workdir: &Path, args: &Value) -> ToolResult {
  let pattern = arg!("find", args, "pattern");
  if pattern.is_empty() {
    return ToolResult::err("find: empty pattern matches everything; refused");
  }
  let path_arg = args.get("path").and_then(Value::as_str).unwrap_or(".");
  let root = path!(workdir, path_arg);
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
  let root = path!(workdir, path_arg);
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
        // canonicalize() failed for every component above, so each re-appended name may still be a
        // symlink the later open() would follow. A dangling link has no canonical form yet escapes
        // the moment the file behind it is created, so refuse rather than re-check later.
        let mut probe = base.clone();
        if let Ok(rel) = resolved.strip_prefix(&base) {
          for part in rel.components() {
            probe.push(part);
            if std::fs::symlink_metadata(&probe)
              .map(|m| m.file_type().is_symlink())
              .unwrap_or(false)
            {
              return Err(format!("path escapes workdir via symlink: {p}"));
            }
          }
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
        // The walk runs on the async runtime, so an unbounded tree both eats memory and stalls
        // every other session's stream while it grinds.
        if files.len() >= WALK_FILE_CAP {
          return Ok(files);
        }
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

/// Trim a captured pipe down to the cap, on a char boundary.
fn cap_string(s: String) -> String {
  if s.len() <= BASH_OUTPUT_CAP {
    return s;
  }
  let mut out = s;
  let cut = floor_char_boundary(&out, BASH_OUTPUT_CAP);
  out.truncate(cut);
  out
}

/// Read a pipe into `buf` until EOF, until the cap, or until it has been quiet for a moment.
/// Partial output is always left in the shared buffer, so the caller can take whatever arrived
/// even if this task is still waiting on a pipe some grandchild is holding.
async fn drain_into(
  mut pipe: impl tokio::io::AsyncRead + Unpin,
  buf: Arc<tokio::sync::Mutex<Vec<u8>>>,
) {
  use tokio::io::AsyncReadExt;
  let mut chunk = [0u8; 8192];
  loop {
    match tokio::time::timeout(std::time::Duration::from_millis(500), pipe.read(&mut chunk)).await {
      Err(_) => continue,
      Ok(Ok(0)) | Ok(Err(_)) => return,
      Ok(Ok(n)) => {
        let mut b = buf.lock().await;
        // One byte past the cap so the caller can tell "exactly at the limit" from "overflowing";
        // the buffer is trimmed back to the cap afterwards.
        let room = (BASH_OUTPUT_CAP + 1).saturating_sub(b.len());
        if room > 0 {
          b.extend_from_slice(&chunk[..n.min(room)]);
        }
        // Past the cap we keep reading purely to discard. Stopping here would leave the child
        // blocked on a full pipe forever, turning "produced too much output" into a timeout --
        // and re-creating the SIGPIPE failure this whole path exists to avoid.
      }
    }
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

fn floor_char_boundary(text: &str, mut at: usize) -> usize {
  let bytes = text.as_bytes();
  at = at.min(bytes.len());
  while at > 0 && !text.is_char_boundary(at) {
    at -= 1;
  }
  at
}

#[cfg(test)]
mod tests {
  use std::fs;

  use serde_json::{Value, json};

  use super::*;

  fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("slopfile-tools-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
  }

  async fn t(name: &str, args: Value, dir: PathBuf) -> ToolResult {
    run(name, &args.to_string(), dir, 30_000, &[], None).await
  }

  const TRANSCRIPT: &str = "```toml\nmodel = \"m\"\nthinking = \"auto\"\n```\n<!-- slopfile:begin -->\n<!-- slopfile:system -->\nSys.\n<!-- /slopfile -->\n<!-- slopfile:turn -->\n<!-- slopfile:user -->\nhi\n<!-- /slopfile -->\n<!-- slopfile:assistant -->\nyo\n<!-- /slopfile -->\n<!-- /slopfile -->\n<!-- slopfile:input -->\n";

  async fn ctx(name: &str, args: Value, wd: PathBuf, context: &Path) -> ToolResult {
    run(name, &args.to_string(), wd, 30_000, &[], Some(context)).await
  }

  #[tokio::test]
  async fn context_file_is_readable_editable_and_writable() {
    let wd = test_dir("ctx-access");
    let outside = test_dir("ctx-access-outside");
    let context = outside.join("s.md");
    fs::write(&context, TRANSCRIPT).expect("seed transcript");

    let read = ctx(
      "read",
      json!({"path": context.to_string_lossy()}),
      wd.clone(),
      &context,
    )
    .await;
    assert!(
      read.ok,
      "own context file must be readable: {}",
      read.output
    );
    assert!(
      read.output.contains("<!-- slopfile:user -->"),
      "{}",
      read.output
    );

    let edit = ctx(
      "edit",
      json!({
        "path": context.to_string_lossy(),
        "old": "yo",
        "new": "NOTES: kept"
      }),
      wd.clone(),
      &context,
    )
    .await;
    assert!(edit.ok, "context edit failed: {}", edit.output);
    let after = fs::read_to_string(&context).unwrap();
    assert!(after.contains("NOTES: kept"), "{after}");

    let write = ctx(
      "write",
      json!({
        "path": context.to_string_lossy(),
        "content": "```toml\nmodel = \"m\"\n```\n<!-- slopfile:begin -->\n<!-- slopfile:system -->\nSys.\n<!-- /slopfile -->\n<!-- slopfile:input -->\n"
      }),
      wd.clone(),
      &context,
    )
    .await;
    assert!(write.ok, "context write failed: {}", write.output);

    let sibling = ctx(
      "write",
      json!({"path": outside.join("other.md").to_string_lossy(), "content": "x"}),
      wd.clone(),
      &context,
    )
    .await;
    assert!(
      !sibling.ok,
      "only the context file is exempt: {}",
      sibling.output
    );

    let _ = fs::remove_dir_all(&wd);
    let _ = fs::remove_dir_all(&outside);
  }

  #[tokio::test]
  async fn context_edits_that_would_break_the_session_are_refused() {
    let wd = test_dir("ctx-refuse");
    let outside = test_dir("ctx-refuse-outside");
    let context = outside.join("s.md");
    fs::write(&context, TRANSCRIPT).expect("seed transcript");

    let no_header = ctx(
      "write",
      json!({"path": context.to_string_lossy(), "content": "<!-- slopfile:user -->\nhi\n<!-- /slopfile -->\n"}),
      wd.clone(),
      &context,
    )
    .await;
    assert!(!no_header.ok, "{}", no_header.output);
    assert!(
      no_header.output.contains("toml header"),
      "{}",
      no_header.output
    );
    assert_eq!(
      fs::read_to_string(&context).unwrap(),
      TRANSCRIPT,
      "refused write must leave the context alone"
    );

    let no_turns = ctx(
      "write",
      json!({"path": context.to_string_lossy(), "content": "```toml\nmodel = \"m\"\n```\n<!-- slopfile:begin -->\n<!-- slopfile:input -->\n"}),
      wd.clone(),
      &context,
    )
    .await;
    assert!(!no_turns.ok, "{}", no_turns.output);
    assert!(no_turns.output.contains("system"), "{}", no_turns.output);

    let header_killer = ctx(
      "edit",
      json!({"path": context.to_string_lossy(), "old": "```toml\nmodel = \"m\"", "new": "# S"}),
      wd.clone(),
      &context,
    )
    .await;
    assert!(!header_killer.ok, "{}", header_killer.output);
    assert!(
      header_killer.output.contains("refused"),
      "{}",
      header_killer.output
    );
    assert!(fs::read_to_string(&context).unwrap().contains("```toml"));

    let _ = fs::remove_dir_all(&wd);
    let _ = fs::remove_dir_all(&outside);
  }

  #[tokio::test]
  async fn reads_are_not_gated_on_file_contents() {
    let wd = test_dir("read-any");
    let transcript =
      "```toml\nmodel = \"m\"\n```\n<!-- slopfile:begin -->\n<!-- slopfile:input -->\n";
    t(
      "write",
      json!({"path": "transcript.md", "content": transcript}),
      wd.clone(),
    )
    .await;
    let w2 = t(
      "write",
      json!({"path": "plain.md", "content": "just text\n"}),
      wd.clone(),
    )
    .await;
    assert!(w2.ok, "write plain failed: {}", w2.output);

    let r = t("read", json!({"path": "transcript.md"}), wd.clone()).await;
    assert!(r.ok, "a region file is ordinary text now: {}", r.output);
    assert!(r.output.contains("slopfile:input"), "{}", r.output);

    let ok = t("read", json!({"path": "plain.md"}), wd.clone()).await;
    assert!(ok.ok, "plain read should succeed: {}", ok.output);
  }

  #[tokio::test]
  async fn read_allows_extra_roots_but_only_for_read() {
    let wd = test_dir("extra-roots-wd");
    let skills = test_dir("extra-roots-skills");
    fs::write(skills.join("skill.md"), "# Skill\nbody line\n").unwrap();

    let args = json!({"path": skills.join("skill.md").to_string_lossy()});
    let refused = run("read", &args.to_string(), wd.clone(), 30_000, &[], None).await;
    assert!(
      !refused.ok,
      "read outside workdir without extra root must fail"
    );

    let allowed = run(
      "read",
      &args.to_string(),
      wd.clone(),
      30_000,
      std::slice::from_ref(&skills),
      None,
    )
    .await;
    assert!(
      allowed.ok,
      "read with extra root should succeed: {}",
      allowed.output
    );
    assert!(allowed.output.contains("body line"), "{}", allowed.output);

    let escaped_write = run(
      "write",
      &json!({"path": skills.join("evil.md").to_string_lossy(), "content": "x"}).to_string(),
      wd.clone(),
      30_000,
      std::slice::from_ref(&skills),
      None,
    )
    .await;
    assert!(
      !escaped_write.ok,
      "write must never use extra roots: {}",
      escaped_write.output
    );
    assert!(
      !skills.join("evil.md").exists(),
      "no file may be written via extra root"
    );

    let _ = fs::remove_dir_all(&wd);
    let _ = fs::remove_dir_all(&skills);
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
  async fn grep_truncates_oversized_matching_lines() {
    let wd = test_dir("grep-longline");
    fs::write(
      wd.join("big.json"),
      format!("{{\"k\":\"{}\"}}\n", "x".repeat(10_000)),
    )
    .unwrap();
    let r = t("grep", json!({"pattern": "\"k\""}), wd.clone()).await;
    assert!(r.ok, "grep failed: {}", r.output);
    assert!(
      r.output.contains("line truncated"),
      "long line must be capped: {}",
      r.output
    );
    assert!(
      r.output.len() < 10_000,
      "output must stay bounded: {}",
      r.output.len()
    );
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
    let r = run(
      "bash",
      "{\"command\":\"sleep 5\"}",
      wd.clone(),
      200,
      &[],
      None,
    )
    .await;
    assert!(!r.ok, "sleep 5 with 200ms timeout should have timed out");
    assert!(r.output.contains("timeout"), "unexpected: {}", r.output);
    let _ = fs::remove_dir_all(&wd);
  }

  #[tokio::test]
  async fn bash_timeout_kills_backgrounded_processes() {
    let wd = test_dir("bash-bgkill");
    let pid_file = wd.join("bg.pid");
    // Backgrounding alone used to hang the call; the foreground part has to hang too, so this
    // still exercises the timeout path and the process-group kill.
    let cmd = format!("sleep 60 & echo $! > {}; sleep 60", pid_file.display());
    let r = run(
      "bash",
      &json!({ "command": cmd }).to_string(),
      wd.clone(),
      200,
      &[],
      None,
    )
    .await;
    assert!(!r.ok, "should have timed out");
    assert!(r.output.contains("timeout"), "unexpected: {}", r.output);
    let bg_pid: i32 = fs::read_to_string(&pid_file)
      .expect("background pid file")
      .trim()
      .parse()
      .expect("pid is a number");
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
        "read", "write", "edit", "bash", "grep", "find", "ls", "subagent", "solve", "question"
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

#[cfg(test)]
mod output_cap_tests {
  use super::*;

  #[tokio::test]
  async fn a_command_that_never_stops_does_not_buffer_without_bound() {
    // `yes` used to be read to EOF before truncation: seconds of unbounded growth in a process
    // that hosts every session.
    let mut child = tokio::process::Command::new("sh")
      .args(["-c", "yes abcdefghijklmnopqrstuvwxyz"])
      .stdout(Stdio::piped())
      .stderr(Stdio::null())
      .spawn()
      .expect("spawn");
    let stdout = child.stdout.take().expect("piped");
    let started = std::time::Instant::now();
    let buf = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    // `yes` never ends, so the drain is only bounded by memory, not by EOF.
    tokio::time::timeout(
      std::time::Duration::from_millis(1500),
      drain_into(stdout, buf.clone()),
    )
    .await
    .expect_err("a drain that only ends at EOF must still be bounded in time");
    let out = String::from_utf8_lossy(&buf.lock().await).into_owned();
    let _ = child.start_kill();
    let _ = child.wait().await;
    assert!(
      out.len() < BASH_OUTPUT_CAP + 200,
      "buffered {} bytes",
      out.len()
    );
    assert!(
      out.len() <= BASH_OUTPUT_CAP + 1,
      "the buffer must stop just past the cap, not grow: {}",
      out.len()
    );
    assert!(
      started.elapsed() < std::time::Duration::from_secs(5),
      "reading must stop at the cap, not at EOF"
    );
  }

  #[tokio::test]
  async fn output_under_the_cap_is_returned_verbatim() {
    let mut child = tokio::process::Command::new("sh")
      .args(["-c", "printf 'small output'"])
      .stdout(Stdio::piped())
      .stderr(Stdio::null())
      .spawn()
      .expect("spawn");
    let buf = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    drain_into(child.stdout.take().expect("piped"), buf.clone()).await;
    let out = String::from_utf8_lossy(&buf.lock().await).into_owned();
    let _ = child.wait().await;
    assert_eq!(out, "small output");
  }
}
