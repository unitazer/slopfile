use clap::{Parser, Subcommand};
use slopfile::{api::LlamaClient, cmd_abort, cmd_init, cmd_tokens, daemon, load_app_config, scan_transcripts};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

const THROTTLE_SECS: u64 = 5;
const PARK_AFTER_FAILURES: u32 = 5;

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

fn acquire_daemon_lock(transcripts_dir: &str) -> std::fs::File {
  let lock_path = Path::new(transcripts_dir).join(".slopfile.lock");
  let file = OpenOptions::new()
    .create(true)
    .write(true)
    .truncate(false)
    .open(&lock_path)
    .unwrap_or_else(|e| {
      eprintln!("slopfile: cannot open {}: {e}", lock_path.display());
      std::process::exit(1);
    });
  if let Err(e) = file.try_lock() {
    eprintln!(
      "slopfile: another daemon already holds {} ({e}); refusing to run two instances",
      lock_path.display()
    );
    std::process::exit(1);
  }
  file
}

fn file_fingerprint(path: &Path) -> Option<(SystemTime, u64)> {
  let meta = std::fs::metadata(path).ok()?;
  Some((meta.modified().ok()?, meta.len()))
}

fn park_transcript(path: &Path) -> Option<SystemTime> {
  let text = std::fs::read_to_string(path).ok()?;
  let (body, live) = slopfile::transcript::strip_live_line(&text);
  let out = slopfile::transcript::reassemble(
    &body,
    &format!("{}\n", slopfile::lex::PARKED_MARKER),
    live.as_deref(),
  );
  std::fs::write(path, out).ok()?;
  let parked_at = file_fingerprint(path).map(|(t, _)| t);
  warn!(
    "session {}: parked after {PARK_AFTER_FAILURES} consecutive failures; edit the file to retry",
    path.display()
  );
  parked_at
}

fn unpark_transcript(path: &Path) {
  let Ok(text) = std::fs::read_to_string(path) else {
    return;
  };
  let cleaned = slopfile::transcript::without_parked_lines(&text);
  if cleaned != text
    && let Err(e) = std::fs::write(path, cleaned)
  {
    warn!("session {}: could not clear parked marker: {e}", path.display());
  }
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

  let _daemon_lock = acquire_daemon_lock(&app_cfg.transcripts_dir);

  let idle_secs = if app_cfg.stream_idle_timeout_secs == 0 {
    u64::MAX
  } else {
    app_cfg.stream_idle_timeout_secs
  };
  let idle_timeout = Duration::from_secs(idle_secs);

  let busy: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
  let throttled: Arc<Mutex<HashMap<PathBuf, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
  let failures: Arc<Mutex<HashMap<PathBuf, u32>>> = Arc::new(Mutex::new(HashMap::new()));
  let parked: Arc<Mutex<HashMap<PathBuf, SystemTime>>> = Arc::new(Mutex::new(HashMap::new()));
  let reported: Arc<Mutex<HashMap<PathBuf, ((SystemTime, u64), bool)>>> =
    Arc::new(Mutex::new(HashMap::new()));

  loop {
    let files = scan_transcripts(&app_cfg.transcripts_dir).unwrap_or_default();
    for path in files {
      if slopfile::flow::is_subagent_path(&path) {
        let fp = file_fingerprint(&path);
        let mut cache = reported.lock().await;
        if fp.is_some_and(|k| cache.get(&path).is_some_and(|(known, done)| *done && *known == k)) {
          continue;
        }
        let done = std::fs::read_to_string(&path)
          .map(|t| t.contains(slopfile::lex::REPORTED_MARKER))
          .unwrap_or(false);
        if let Some(k) = fp {
          cache.insert(path.clone(), (k, done));
        }
        if done {
          continue;
        }
      }
      let parked_at = parked.lock().await.get(&path).copied();
      if let Some(at) = parked_at {
        if file_fingerprint(&path).map(|(t, _)| t) == Some(at) {
          continue;
        }
        parked.lock().await.remove(&path);
        failures.lock().await.remove(&path);
        unpark_transcript(&path);
        info!("session {}: unparked (file changed)", path.display());
      }
      let mut guard = busy.lock().await;
      if guard.contains(&path) {
        continue;
      }
      if let Some(at) = throttled.lock().await.get(&path)
        && at.elapsed() < Duration::from_secs(THROTTLE_SECS)
      {
        continue;
      }
      guard.insert(path.clone());
      drop(guard);

      let cfg = app_cfg.clone();
      let busy = busy.clone();
      let throttled = throttled.clone();
      let failures = failures.clone();
      let parked = parked.clone();
      tokio::spawn(async move {
        let client = LlamaClient::new(&cfg.api_base).with_idle_timeout(idle_timeout);
        match daemon::drive_file(&path, &cfg, &client).await {
          Ok(()) => {
            failures.lock().await.remove(&path);
          }
          Err(e) => {
            error!("session {}: {e}", path.display());
            throttled.lock().await.insert(path.clone(), Instant::now());
            let count = {
              let mut f = failures.lock().await;
              let c = f.entry(path.clone()).or_insert(0);
              *c += 1;
              *c
            };
            if count >= PARK_AFTER_FAILURES {
              failures.lock().await.remove(&path);
              if let Some(at) = park_transcript(&path) {
                parked.lock().await.insert(path.clone(), at);
              }
            }
          }
        }
        busy.lock().await.remove(&path);
      });
    }
    sleep(Duration::from_millis(app_cfg.sweep_interval_ms)).await;
  }
}
