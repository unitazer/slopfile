
mod api;
mod daemon;
mod docker;
mod flow;
mod lex;
mod questions;
mod sse;
mod stream;
mod tokens;
mod tools;
mod transcript;
mod types;

use api::LlamaClient;
use clap::{Parser, Subcommand};
use std::collections::{HashMap, HashSet};
use std::env::var;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};
use types::AppConfig;

#[derive(Parser)]
#[command(
  name = "slopfile",
  about = "file-as-UI coding agent daemon: streams llama-server responses into session .md files"
)]
struct Cli {
  #[command(subcommand)]
  command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
  Init {
    path: String,
  },
  Abort {
    path: String,
  },
  Tokens {
    path: String,
  },
}

/// Locate the repo root by walking up from the executable and the cwd,
/// looking for a directory that has both `shared/` and `Cargo.toml`.
fn discover_repo_root() -> Option<PathBuf> {
  let mut candidates = Vec::new();
  if let Ok(exe) = std::env::current_exe() {
    candidates.extend(exe.ancestors().skip(1).map(Path::to_path_buf));
  }
  if let Ok(cwd) = std::env::current_dir() {
    candidates.extend(cwd.ancestors().map(Path::to_path_buf));
  }
  candidates.into_iter().find(|d| d.join("shared").is_dir() && d.join("Cargo.toml").is_file())
}

fn load_app_config() -> AppConfig {
  let fallback = discover_repo_root()
    .map(|root| root.join("shared").join("slopfile.toml"))
    .unwrap_or_else(|| PathBuf::from("shared/slopfile.toml"));
  let path = var("SLOPFILE_CONFIG").unwrap_or_else(|_| fallback.to_string_lossy().into());
  let mut cfg = AppConfig::load_from_file(&path).unwrap_or_else(|e| {
    warn!("could not load app config ({path}): {e}; using defaults");
    AppConfig::default()
  });
  // Repo-relative defaults for a host launch; the container sets SLOPFILE_*
  // env vars explicitly so this only fires outside docker.
  if let Some(root) = discover_repo_root() {
    if cfg.workdir == "workspace" && root.join("workspace").is_dir() {
      cfg.workdir = root.join("workspace").to_string_lossy().into();
    }
    if cfg.transcripts_dir == "shared/transcripts" && root.join("shared/transcripts").is_dir() {
      cfg.transcripts_dir = root.join("shared/transcripts").to_string_lossy().into();
    }
  }
  if let Ok(v) = var("SLOPFILE_API_BASE") {
    cfg.api_base = v;
  }
  if let Ok(v) = var("SLOPFILE_TRANSCRIPTS") {
    cfg.transcripts_dir = v;
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

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "slopfile=info,warn".into()),
    )
    .init();

  let app_cfg = load_app_config();
  let cli = Cli::parse();
  match cli.command {
    Some(Command::Init { path }) => return cmd_init(&path, &app_cfg),
    Some(Command::Abort { path }) => return cmd_abort(&path),
    Some(Command::Tokens { path }) => return cmd_tokens(&path, &app_cfg).await,
    None => {}
  }

  info!(
    "slopfile: workdir={} transcripts={} model={} api={} sweep={}ms confirmations={:?}",
    app_cfg.workdir,
    app_cfg.transcripts_dir,
    app_cfg.model,
    app_cfg.api_base,
    app_cfg.sweep_interval_ms,
    app_cfg.confirmations
  );
  std::fs::create_dir_all(&app_cfg.transcripts_dir).ok();

  let busy: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
  let throttled: Arc<Mutex<HashMap<PathBuf, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

  loop {
    let files = scan_transcripts(&app_cfg.transcripts_dir).unwrap_or_default();
    for path in files {
      let mut guard = busy.lock().await;
      if guard.contains(&path) {
        continue;
      }
      if let Some(at) = throttled.lock().await.get(&path)
        && at.elapsed() < Duration::from_secs(5)
      {
        continue;
      }
      guard.insert(path.clone());
      drop(guard);

      let cfg = app_cfg.clone();
      let busy = busy.clone();
      let throttled = throttled.clone();
      tokio::spawn(async move {
        let client = LlamaClient::new(&cfg.api_base);
        if let Err(e) = daemon::drive_file(&path, &cfg, &client).await {
          error!("session {}: {e}", path.display());
          throttled.lock().await.insert(path.clone(), Instant::now());
        }
        busy.lock().await.remove(&path);
      });
    }
    sleep(Duration::from_millis(app_cfg.sweep_interval_ms)).await;
  }
}

fn scan_transcripts(dir: &str) -> Result<Vec<PathBuf>, String> {
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

fn cmd_init(path: &str, cfg: &AppConfig) {
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

fn init_template_for(path: &str, cfg: &AppConfig) -> Result<String, String> {
  if let Ok(existing) = std::fs::read_to_string(path)
    && !existing.trim().is_empty()
  {
    return Err(format!("{path} exists and is not empty; refusing to overwrite"));
  }
  Ok(flow::session_template(cfg))
}

fn cmd_abort(path: &str) {
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

async fn cmd_tokens(path: &str, cfg: &AppConfig) {

  let text = match std::fs::read_to_string(path) {
    Ok(t) => t,
    Err(e) => {
      eprintln!("slopfile tokens: cannot read {path}: {e}");
      exit(1);
    }
  };
  let pf = transcript::parse_file(&text);
  let client = LlamaClient::new(&cfg.api_base);
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