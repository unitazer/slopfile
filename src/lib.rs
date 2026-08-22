pub mod api;
pub mod daemon;
pub mod docker;
pub mod flow;
pub mod lex;
pub mod questions;
pub mod research;
mod sse;
pub mod stream;
pub mod tokens;
pub mod tools;
pub mod transcript;
pub mod types;

use std::path::{Path, PathBuf};
use std::process::exit;

pub fn discover_repo_root() -> Option<PathBuf> {
  let mut candidates = Vec::new();
  if let Ok(exe) = std::env::current_exe() {
    candidates.extend(exe.ancestors().skip(1).map(Path::to_path_buf));
  }
  if let Ok(cwd) = std::env::current_dir() {
    candidates.extend(cwd.ancestors().map(Path::to_path_buf));
  }
  candidates.into_iter().find(|d| d.join("shared").is_dir() && d.join("Cargo.toml").is_file())
}

pub fn load_app_config() -> types::AppConfig {
  use std::env::var;
  use tracing::warn;

  let fallback = discover_repo_root()
    .map(|root| root.join("shared").join("slopfile.toml"))
    .unwrap_or_else(|| PathBuf::from("shared/slopfile.toml"));
  let path = var("SLOPFILE_CONFIG").unwrap_or_else(|_| fallback.to_string_lossy().into());
  let mut cfg = types::AppConfig::load_from_file(&path).unwrap_or_else(|e| {
    warn!("could not load app config ({path}): {e}; using defaults");
    types::AppConfig::default()
  });
  if let Some(root) = discover_repo_root() {
    if cfg.workdir == "workspace" && root.join("workspace").is_dir() {
      cfg.workdir = root.join("workspace").to_string_lossy().into();
    }
    if cfg.transcripts_dir == "shared/transcripts" && root.join("shared/transcripts").is_dir() {
      cfg.transcripts_dir = root.join("shared/transcripts").to_string_lossy().into();
    }
    if cfg.skills_dir == "shared/skills" && root.join("shared/skills").is_dir() {
      cfg.skills_dir = root.join("shared/skills").to_string_lossy().into();
    }
  }
  if let Ok(v) = var("SLOPFILE_API_BASE") {
    cfg.api_base = v;
  }
  if let Ok(v) = var("SLOPFILE_TRANSCRIPTS") {
    cfg.transcripts_dir = v;
  }
  if let Ok(v) = var("SLOPFILE_SKILLS_DIR") {
    cfg.skills_dir = v;
  }
  if let Ok(v) = var("SLOPFILE_WORKDIR") {
    cfg.workdir = v;
  }
  if let Ok(v) = var("SLOPFILE_ABORT_RESTART") {
    cfg.abort_restart_server = v != "0" && !v.eq_ignore_ascii_case("false");
  }
  if let Ok(v) = var("SLOPFILE_LLAMA_CONTAINER") {
    cfg.llama_container = v;
  }
  cfg
}

pub fn scan_transcripts(dir: &str) -> Result<Vec<PathBuf>, String> {
  let mut out = Vec::new();
  for entry in std::fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))? {
    let entry = entry.map_err(|e| e.to_string())?;
    let p = entry.path();
    if p.is_file() && p.extension().is_some_and(|e| e == "md") {
      out.push(p);
    }
  }
  out.sort();
  Ok(out)
}

pub fn cmd_init(path: &str, cfg: &types::AppConfig) {
  match init_template_for(path, cfg) {
    Ok(content) => match std::fs::write(path, content) {
      Ok(()) => println!("initialized {path}"),
      Err(e) => {
        eprintln!("slopfile init: cannot write {path}: {e}");
        exit(1);
      }
    },
    Err(e) => {
      eprintln!("slopfile init: {e}");
      exit(1);
    }
  }
}

fn init_template_for(path: &str, cfg: &types::AppConfig) -> Result<String, String> {
  if Path::new(path)
    .file_stem()
    .is_some_and(|s| s.to_string_lossy().contains(".sub-"))
  {
    return Err(format!("{path}: transcript names must not contain '.sub-' (reserved for subagents)"));
  }
  if let Ok(existing) = std::fs::read_to_string(path)
    && !existing.trim().is_empty()
  {
    return Err(format!("{path} exists and is not empty; refusing to overwrite"));
  }
  Ok(flow::session_template(cfg))
}

pub fn cmd_abort(path: &str) {
  let content = match std::fs::read_to_string(path) {
    Ok(c) => c,
    Err(e) => {
      eprintln!("slopfile abort: cannot read {path}: {e}");
      exit(1);
    }
  };
  if transcript::abort_marker_present(&content) {
    println!("abort already requested on {path}");
    return;
  }
  let mut out = content;
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(transcript::ABORT_MARKER);
  out.push('\n');
  if let Err(e) = std::fs::write(path, out) {
    eprintln!("slopfile abort: cannot write {path}: {e}");
    exit(1);
  }
  println!("abort requested on {path}; the daemon stops the turn on its next sweep");
}

pub async fn cmd_tokens(path: &str, cfg: &types::AppConfig) {
  let text = match std::fs::read_to_string(path) {
    Ok(t) => t,
    Err(e) => {
      eprintln!("slopfile tokens: cannot read {path}: {e}");
      exit(1);
    }
  };
  let pf = transcript::parse_file(&text);
  let client = api::LlamaClient::new(&cfg.api_base);
  let plan = match tokens::build_request_messages(&text, &pf, cfg, client.rest()).await {
    Ok(v) => v,
    Err(e) => {
      eprintln!("slopfile tokens: {e}");
      exit(1);
    }
  };
  let context_budget = pf
    .config
    .max_context_tokens
    .unwrap_or(cfg.max_context_tokens);

  let headroom = context_budget.saturating_sub(plan.token_count as u32);

  println!("tokens: {path}");
  println!("  messages:    {} sent ({} oldest dropped to fit budget)", plan.messages.len(), plan.dropped);
  println!("  model tokens: {}  (via llama-server /tokenize, content only)", plan.token_count);
  println!("  context budget: {context_budget}  output allowance: {}", context_budget / 4);
  println!("  input headroom: {headroom}");
}

#[cfg(test)]
mod main_tests;
