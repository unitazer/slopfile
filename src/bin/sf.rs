use clap::{Parser, Subcommand};
use env_logger::Env;
use log::info;
use slopfile::api::LlamaClient;
use slopfile::control::{self, Cmd, Req};
use slopfile::tokens;
use slopfile::transcript;
use slopfile::types::AppConfig;
use std::path::{Path, PathBuf};
use std::process::exit;

#[derive(Parser)]
#[command(name = "sf", about = "talk to a running slopfile daemon", version)]
struct Cli {
  #[arg(long, global = true)]
  addr: Option<String>,
  #[arg(long, global = true)]
  transcripts: Option<PathBuf>,
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
  /// Create a new session file
  Init { session: String },
  /// Arm the session and start generating
  Send { session: String },
  /// Let the current turn finish, then go idle
  Stop { session: String },
  /// Kill the in-flight stream right now
  Abort { session: String },
  /// Show what the daemon is doing with the session
  Status { session: String },
  /// Answer the questions the model asked
  Question {
    session: String,
    answers: Vec<String>,
  },
  /// Report the token budget for a session
  Tokens { session: String },
}

#[tokio::main]
async fn main() {
  env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
  let cli = Cli::parse();
  let cfg = slopfile::load_app_config();
  let addr = cli
    .addr
    .or_else(|| std::env::var("SF_ADDR").ok())
    .unwrap_or_else(|| control::DEFAULT_ADDR.to_string());
  let transcripts = cli
    .transcripts
    .map(|p| p.to_string_lossy().into_owned())
    .or_else(|| std::env::var("SLOPFILE_TRANSCRIPTS").ok())
    .unwrap_or(cfg.transcripts_dir.clone());

  match run(cli.command, &addr, &transcripts, &cfg).await {
    Ok(()) => {}
    Err(why) => {
      eprintln!("{why}");
      exit(1);
    }
  }
}

async fn run(
  command: Command,
  addr: &str,
  transcripts: &str,
  cfg: &AppConfig,
) -> Result<(), String> {
  match command {
    Command::Init { session } => init(&session, transcripts, cfg),
    Command::Tokens { session } => tokens(&session, transcripts, cfg).await,
    Command::Send { session } => remote(addr, Cmd::Send, &session, vec![]).await,
    Command::Stop { session } => remote(addr, Cmd::Stop, &session, vec![]).await,
    Command::Abort { session } => remote(addr, Cmd::Abort, &session, vec![]).await,
    Command::Status { session } => remote(addr, Cmd::Status, &session, vec![]).await,
    Command::Question { session, answers } => remote(addr, Cmd::Question, &session, answers).await,
  }
}

async fn remote(addr: &str, cmd: Cmd, session: &str, answers: Vec<String>) -> Result<(), String> {
  with_timeout(remote_inner(addr, cmd, session, answers)).await
}

async fn with_timeout<F: std::future::Future<Output = Result<(), String>>>(
  f: F,
) -> Result<(), String> {
  match tokio::time::timeout(std::time::Duration::from_secs(10), f).await {
    Ok(r) => r,
    Err(_) => Err("control: the daemon did not reply within 10s (is it running?)".to_string()),
  }
}

async fn remote_inner(
  addr: &str,
  cmd: Cmd,
  session: &str,
  answers: Vec<String>,
) -> Result<(), String> {
  let req = Req {
    cmd,
    session: session.to_string(),
    answers,
  };
  let resp = control::send_command(addr, &req)
    .await
    .map_err(|e| e.to_string())?;
  if !resp.ok {
    return Err(resp.error.unwrap_or_else(|| "command failed".to_string()));
  }
  if let Some(status) = &resp.status
    && cmd == Cmd::Status
  {
    println!("{status}");
  }
  info!("command `{}` sent to task `{}`", cmd_name(cmd), session);
  Ok(())
}

fn cmd_name(cmd: Cmd) -> &'static str {
  match cmd {
    Cmd::Send => "send",
    Cmd::Stop => "stop",
    Cmd::Abort => "abort",
    Cmd::Status => "status",
    Cmd::Question => "question",
  }
}

/// Creating a session is pure local file work, and must work with no daemon running. The only hard
/// part is finding the transcripts directory, which `load_app_config` now resolves for the host.
fn init(session: &str, transcripts: &str, cfg: &AppConfig) -> Result<(), String> {
  let path = local_path(session, transcripts);
  if let Some(dir) = path.parent()
    && !dir.as_os_str().is_empty()
  {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
  }
  control::create_session(&path, cfg, &[])?;
  info!("initialized task `{}`", path.display());
  Ok(())
}

fn local_path(session: &str, transcripts: &str) -> PathBuf {
  let asked = Path::new(session);
  if session.contains('/') || asked.exists() {
    return asked.to_path_buf();
  }
  let stem = session.strip_suffix(".md").unwrap_or(session);
  Path::new(transcripts).join(format!("{stem}.md"))
}

async fn tokens(session: &str, transcripts: &str, cfg: &AppConfig) -> Result<(), String> {
  let path = local_path(session, transcripts);
  if !path.exists() {
    return Err(format!("no such task `{}`", path.display()));
  }
  let parsed = transcript::load(&path)?;
  let client = LlamaClient::new(cfg).for_provider(cfg, parsed.provider(), parsed.headers())?;
  let messages = parsed.request_messages();
  let tool_defs = slopfile::tools::advertised(&parsed.config);
  let tools_chars = serde_json::to_string(&tool_defs).map_or(0, |s| s.len() as u64);
  // No anchor here: the measured sample lives in the daemon's memory for the session it belongs
  // to. Running this command tells you what the payload weighs, not what it costs in tokens --
  // except against a llama-server, which counts for real and needs no sample.
  let plan = slopfile::tokens::build_request_messages(
    &parsed,
    messages,
    cfg,
    client.endpoint(),
    &client,
    &slopfile::tokens::Counting {
      anchor: None,
      tools_chars,
    },
  )
  .await
  .map_err(|e| format!("tokens: {e}"))?;
  let budget = slopfile::tokens::context_budget(&parsed, cfg);
  println!("tokens: {}", path.display());
  println!(
    "  messages:     {} sent ({} oldest dropped to fit budget)",
    plan.messages.len(),
    plan.dropped
  );
  match plan.token_count {
    Some(n) => {
      // Against `budget`, not the usable prompt: `n` is already capped at `prompt_budget`, so
      // subtracting from the full window would never drop below the output allowance and would
      // report headroom on a transcript that has none left.
      let headroom = tokens::prompt_budget(budget).saturating_sub(n as u32);
      println!(
        "  model tokens: {n}  (via {}/tokenize)",
        client.endpoint().base
      );
      println!("  headroom:     {headroom} tokens");
    }
    None => println!(
      "  model tokens: unknown -- {} has no tokenizer, and the measured sample lives in the \
       daemon. Payload is {} bytes; run the daemon and read its log for a real count.",
      client.endpoint().base,
      plan.payload_chars
    ),
  }
  let allowance = slopfile::tokens::output_allowance(budget);
  println!(
    "  context budget: {budget}  output allowance: {allowance}  prompt budget: {}",
    budget.saturating_sub(allowance)
  );
  info!("command `tokens` reported for task `{}`", path.display());
  Ok(())
}
