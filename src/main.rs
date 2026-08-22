use clap::{Parser, Subcommand};
use slopfile::{api::LlamaClient, cmd_abort, cmd_init, cmd_tokens, daemon, load_app_config, scan_transcripts};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info};

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
      if slopfile::flow::is_subagent_path(&path)
        && std::fs::read_to_string(&path)
          .map(|t| t.contains(slopfile::lex::REPORTED_MARKER))
          .unwrap_or(false)
      {
        continue;
      }
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
