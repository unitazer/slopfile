use clap::{Parser, Subcommand};
use slopfile::control::{Cmd, Req};
use slopfile::{api::LlamaClient, control, daemon, load_app_config, scan_transcripts};
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
  /// Write a new session file (local, no daemon needed)
  Init {
    path: String,
  },
  /// Arm and generate; same commands as the host `sf` CLI, for use inside the container
  Send {
    session: String,
  },
  Abort {
    session: String,
  },
  Stop {
    session: String,
  },
  Status {
    session: String,
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

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "slopfile=info,warn".into()),
    )
    .init();

  let app_cfg = load_app_config();
  let addr = control::addr_from_env();
  let cli = Cli::parse();
  if let Some(command) = cli.command {
    return dispatch(command, &app_cfg, &addr).await;
  }

  info!(
    "slopfile: workdir={} transcripts={} model={} api={} sweep={}ms confirmations={:?} control={addr}",
    app_cfg.workdir,
    app_cfg.transcripts_dir,
    app_cfg.model,
    app_cfg.api_base,
    app_cfg.sweep_interval_ms,
    app_cfg.confirmations
  );
  std::fs::create_dir_all(&app_cfg.transcripts_dir).ok();

  let _daemon_lock = acquire_daemon_lock(&app_cfg.transcripts_dir);

  {
    let cfg = app_cfg.clone();
    let bind = addr.clone();
    tokio::spawn(async move {
      if let Err(e) = control::serve(&bind, cfg).await {
        error!("control channel is down: {e}");
      }
    });
  }

  let idle_secs = if app_cfg.stream_idle_timeout_secs == 0 {
    u64::MAX
  } else {
    app_cfg.stream_idle_timeout_secs
  };
  let idle_timeout = Duration::from_secs(idle_secs);

  let busy: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
  let throttled: Arc<Mutex<HashMap<PathBuf, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
  let failures: Arc<Mutex<HashMap<PathBuf, u32>>> = Arc::new(Mutex::new(HashMap::new()));
  let parked: Arc<Mutex<HashMap<PathBuf, Option<SystemTime>>>> =
    Arc::new(Mutex::new(HashMap::new()));

  loop {
    // A sweep that cannot list the directory drives nothing, and must say so rather than looking
    // identical to "no sessions exist".
    let files = match scan_transcripts(&app_cfg.transcripts_dir) {
      Ok(files) => files,
      Err(e) => {
        error!("cannot scan {}: {e}; no session will be driven this sweep", app_cfg.transcripts_dir);
        continue;
      }
    };
    // Entries for files that no longer exist are never revisited, so they would accumulate forever.
    {
      let live: std::collections::HashSet<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
      let mut throttled = throttled.lock().await;
      throttled.retain(|p, _| live.contains(p.as_path()));
      drop(throttled);
      parked.lock().await.retain(|p, _| live.contains(p.as_path()));
      failures.lock().await.retain(|p, _| live.contains(p.as_path()));
    }
    for path in files {
      let name = control::session_name(&path);
      if control::snapshot(&name).delivered {
        continue;
      }
      let parked_at = parked.lock().await.get(&path).copied();
      if let Some(at) = parked_at {
        if at.is_some() && file_fingerprint(&path).map(|(t, _)| t) == at {
          continue;
        }
        parked.lock().await.remove(&path);
        failures.lock().await.remove(&path);
        control::update(&name, |s| s.parked = false);
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
              let parked_at = file_fingerprint(&path).map(|(t, _)| t);
              parked.lock().await.insert(path.clone(), parked_at);
              control::update(&name, |s| s.parked = true);
              warn!(
                "session {}: parked after {PARK_AFTER_FAILURES} consecutive failures; edit the file to retry",
                path.display()
              );
            }
          }
        }
        busy.lock().await.remove(&path);
      });
    }
    sleep(Duration::from_millis(app_cfg.sweep_interval_ms)).await;
  }
}

async fn dispatch(command: Command, cfg: &slopfile::types::AppConfig, addr: &str) {
  let (cmd, session, answers) = match command {
    Command::Init { path } => {
      match slopfile::cmd_init(&path, cfg) {
        Ok(()) => return,
        Err(e) => {
          eprintln!("{e}");
          std::process::exit(1);
        }
      }
    }
    Command::Send { session } => (Cmd::Send, session, vec![]),
    Command::Abort { session } => (Cmd::Abort, session, vec![]),
    Command::Stop { session } => (Cmd::Stop, session, vec![]),
    Command::Status { session } => (Cmd::Status, session, vec![]),
  };
  let req = Req {
    cmd,
    session,
    answers,
  };
  match control::send_command(addr, &req).await {
    Ok(resp) if resp.ok => {
      if cmd == Cmd::Status
        && let Some(status) = resp.status
      {
        println!("{status}");
      }
    }
    Ok(resp) => {
      eprintln!("{}", resp.error.unwrap_or_else(|| "command failed".into()));
      std::process::exit(1);
    }
    Err(e) => {
      eprintln!("{e}");
      std::process::exit(1);
    }
  }
}
