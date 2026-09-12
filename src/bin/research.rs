use clap::Parser;
use slopfile::research::{self, ResearchOpts};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
  name = "slopfile-research",
  about = "autoresearch driver: one LLM iteration per fresh transcript, numeric score decides commit or restore"
)]
struct Cli {
  #[arg(long)]
  workdir: PathBuf,
  #[arg(long, default_value = "transcripts")]
  transcripts_dir: PathBuf,
  #[arg(long, default_value = "cargo run --release --bin eval")]
  eval_cmd: String,
  #[arg(long)]
  maximize: bool,
  #[arg(long, default_value_t = 100)]
  max_iters: u32,
  #[arg(long)]
  model: Option<String>,
  #[arg(long, default_value_t = 600)]
  eval_timeout_secs: u64,
  #[arg(long, default_value_t = 7200)]
  iter_timeout_secs: u64,
}

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "slopfile=info,warn".into()),
    )
    .init();

  let cli = Cli::parse();
  let model = cli.model.unwrap_or_else(load_model_default);
  let opts = ResearchOpts {
    workdir: cli.workdir,
    transcripts_dir: cli.transcripts_dir,
    model,
    eval_cmd: cli.eval_cmd,
    maximize: cli.maximize,
    max_iters: cli.max_iters,
    eval_timeout_secs: cli.eval_timeout_secs,
    iter_timeout_secs: cli.iter_timeout_secs,
    poll_ms: 2_000,
  };

  if let Err(e) = research::run(opts).await {
    eprintln!("slopfile-research: {e}");
    std::process::exit(1);
  }
}

fn load_model_default() -> String {
  let cfg = slopfile::load_app_config();
  if cfg.model.is_empty() {
    "default".to_string()
  } else {
    cfg.model
  }
}
