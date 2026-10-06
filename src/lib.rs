pub mod api;
pub mod context;
pub mod control;
pub mod daemon;
pub mod docker;
pub mod flow;
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

pub fn cmd_init(path: &str, cfg: &types::AppConfig) -> Result<(), String> {
  control::create_session(Path::new(path), cfg, &[])?;
  println!("initialized {path}");
  Ok(())
}

#[cfg(test)]
mod main_tests;
