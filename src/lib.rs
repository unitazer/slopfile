pub mod api;
pub mod context;
pub mod control;
pub mod daemon;
pub mod docker;
pub mod flow;
pub mod prompt;
pub mod questions;
pub mod research;
pub mod session;
mod sse;
pub mod stream;
pub mod tokens;
pub mod tools;
pub mod transcript;
pub mod types;

use std::path::{Path, PathBuf};

pub fn discover_repo_root() -> Option<PathBuf> {
  let mut candidates = Vec::new();
  if let Ok(exe) = std::env::current_exe() {
    candidates.extend(exe.ancestors().skip(1).map(Path::to_path_buf));
  }
  if let Ok(cwd) = std::env::current_dir() {
    candidates.extend(cwd.ancestors().map(Path::to_path_buf));
  }
  candidates
    .into_iter()
    .find(|d| d.join("shared").is_dir() && d.join("Cargo.toml").is_file())
}

/// Re-point a `shared/...` config value at the repo we were launched from. `/shared/transcripts`
/// (what the container wants) and `shared/transcripts` (what the host wants) are the same directory
/// bind-mounted, so the leading slash is the only difference. Anything else is left untouched.
pub fn repo_relative(value: &str, root: &std::path::Path) -> String {
  let trimmed = value.trim_start_matches('/');
  if trimmed != value && !trimmed.starts_with("shared/") {
    return value.to_string();
  }
  if trimmed == value && !value.starts_with("shared/") {
    return value.to_string();
  }
  root.join(trimmed).to_string_lossy().into()
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
    // The one config file serves both sides: the daemon sees it as `/shared/transcripts` inside the
    // container, while `sf` runs on the host where that path does not exist (and is not writable).
    // Re-point any `shared/...` value at the repo we were actually run from, leading slash or not.
    cfg.transcripts_dir = repo_relative(&cfg.transcripts_dir, &root);
    cfg.skills_dir = repo_relative(&cfg.skills_dir, &root);
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

pub fn cmd_init(path: &str, cfg: &types::AppConfig) -> Result<(), String> {
  control::create_session(Path::new(path), cfg, &[])?;
  println!("initialized {path}");
  Ok(())
}

#[cfg(test)]
mod main_tests;
