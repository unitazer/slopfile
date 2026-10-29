use crate::flow;
use crate::questions;

use crate::transcript;
use crate::types::{AppConfig, PromptSample};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

pub const DEFAULT_ADDR: &str = "127.0.0.1:8787";
/// The control port has no authentication, so a request has to be small and a client has to
/// actually send one: neither bound is reachable through the session commands themselves.
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_SESSION_NAME: usize = 128;
const CONTROL_READ_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone)]
pub struct SessionState {
  pub armed: bool,
  pub abort: bool,
  pub parked: bool,
  pub running: bool,
  pub context_full: bool,
  pub delivered: bool,
  pub phase: Phase,
  pub pending_questions: usize,
  pub model: String,
  /// The endpoint this session is talking to, so `sf status` can answer "which one is this?"
  pub endpoint: String,
  /// The last request as the provider counted it. In-memory on purpose: it is a cache of an
  /// observation, and a daemon restart costs exactly one uncalibrated turn.
  pub last_prompt: Option<PromptSample>,
  /// The provider refused the request and retrying will not help. Unlike a failure this is
  /// terminal on its own: the session stops immediately instead of being re-driven every sweep
  /// until the park threshold, which against a spent quota is the difference between one wasted
  /// request and several hundred.
  pub blocked: bool,
  /// The user ran `sf stop`. An edit to the file un-parks a session, and without this that edit
  /// would quietly restart something they had deliberately stopped.
  pub stopped: bool,
  arm_token: u64,
}

impl Default for SessionState {
  fn default() -> Self {
    Self {
      armed: false,
      abort: false,
      parked: false,
      running: false,
      context_full: false,
      delivered: false,
      phase: Phase::Idle,
      pending_questions: 0,
      model: String::new(),
      endpoint: String::new(),
      last_prompt: None,
      blocked: false,
      stopped: false,
      arm_token: 0,
    }
  }
}

impl SessionState {
  pub fn arm(&mut self) {
    self.armed = true;
    self.parked = false;
    // An explicit stop is the user saying "not this one". Re-arming it because the file happened
    // to change is how a stopped session comes back to life on its own.
    self.stopped = false;
    // A send is the user saying "try again", which is also how a provider refusal gets cleared.
    self.blocked = false;
    self.arm_token += 1;
  }

  pub fn disarm(&mut self) {
    self.armed = false;
    self.stopped = true;
  }

  pub fn request_abort(&mut self) {
    self.abort = true;
  }

  pub fn take_abort(&mut self) -> bool {
    std::mem::take(&mut self.abort)
  }

  pub fn clear_abort(&mut self) {
    self.abort = false;
  }

  pub fn arm_token(&self) -> u64 {
    self.arm_token
  }

  /// Called when the daemon goes idle: disarms unless a `send` landed after this burst started.
  pub fn settle(&mut self, token_at_entry: u64) {
    if self.arm_token == token_at_entry {
      self.armed = false;
    }
  }

  pub fn status(&self, name: &str) -> Status {
    Status {
      session: name.to_string(),
      phase: self.phase,
      running: self.running,
      armed: self.armed,
      abort: self.abort,
      parked: self.parked,
      blocked: self.blocked,
      stopped: self.stopped,
      context_full: self.context_full,
      pending_questions: self.pending_questions,
      model: self.model.clone(),
      endpoint: self.endpoint.clone(),
    }
  }
}

pub type Registry = Arc<Mutex<HashMap<String, SessionState>>>;

fn registry() -> &'static Registry {
  static REGISTRY: OnceLock<Registry> = OnceLock::new();
  REGISTRY.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

pub fn update<R>(name: &str, f: impl FnOnce(&mut SessionState) -> R) -> R {
  let mut guard = registry().lock().unwrap_or_else(|e| e.into_inner());
  f(guard.entry(name.to_string()).or_default())
}

pub fn snapshot(name: &str) -> SessionState {
  let guard = registry().lock().unwrap_or_else(|e| e.into_inner());
  guard.get(name).cloned().unwrap_or_default()
}

pub fn session_name(path: &Path) -> String {
  path
    .file_stem()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Cmd {
  Send,
  Abort,
  Stop,
  Status,
  Question,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Req {
  pub cmd: Cmd,
  pub session: String,
  #[serde(default)]
  pub answers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
  #[default]
  Idle,
  Generating,
  RunningTools,
  WaitingAnswer,
  WaitingSubagents,
}

impl Phase {
  pub fn label(self) -> &'static str {
    match self {
      Self::Idle => "idle",
      Self::Generating => "generating",
      Self::RunningTools => "running_tools",
      Self::WaitingAnswer => "waiting_answer",
      Self::WaitingSubagents => "waiting_subagents",
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
  pub session: String,
  pub phase: Phase,
  pub running: bool,
  pub armed: bool,
  pub abort: bool,
  pub parked: bool,
  /// The provider refused this session outright, so it is parked for a different reason than the
  /// retry threshold. Worth its own field: a user reading `parked=true` cannot otherwise tell a
  /// five-failure blip from a rate limit that will not clear on its own.
  pub blocked: bool,
  /// The user asked for this session to stop. Un-parking must not quietly undo that.
  pub stopped: bool,
  pub context_full: bool,
  pub pending_questions: usize,
  pub model: String,
  pub endpoint: String,
}

impl std::fmt::Display for Status {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "task {} phase={} running={} armed={} abort={} parked={} blocked={} stopped={} context_full={} pending_questions={} model={} endpoint={}",
      self.session,
      self.phase.label(),
      self.running,
      self.armed,
      self.abort,
      self.parked,
      self.blocked,
      self.stopped,
      self.context_full,
      self.pending_questions,
      self.model,
      self.endpoint
    )
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resp {
  pub ok: bool,
  #[serde(default)]
  pub error: Option<String>,
  #[serde(default)]
  pub status: Option<Status>,
}

impl Resp {
  pub fn done(status: Status) -> Self {
    Self {
      ok: true,
      error: None,
      status: Some(status),
    }
  }

  pub fn failed(error: impl Into<String>) -> Self {
    Self {
      ok: false,
      error: Some(error.into()),
      status: None,
    }
  }
}

#[derive(Debug)]
pub enum ControlError {
  Connect(String),
  Protocol(String),
  Failed(String),
}

impl std::fmt::Display for ControlError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Connect(_) => write!(
        f,
        "unable to establish tcp connection, is the daemon running?"
      ),
      Self::Protocol(m) => write!(f, "{m}"),
      Self::Failed(m) => write!(f, "{m}"),
    }
  }
}

pub async fn send_command(addr: &str, req: &Req) -> Result<Resp, ControlError> {
  let mut stream = TcpStream::connect(addr)
    .await
    .map_err(|_| ControlError::Connect(addr.to_string()))?;
  let mut line = serde_json::to_string(req).unwrap_or_default();
  line.push('\n');
  stream
    .write_all(line.as_bytes())
    .await
    .map_err(|_| ControlError::Connect(addr.to_string()))?;
  let mut reply = String::new();
  BufReader::new(stream)
    .read_line(&mut reply)
    .await
    .map_err(|_| ControlError::Connect(addr.to_string()))?;
  if reply.trim().is_empty() {
    return Err(ControlError::Protocol(
      "daemon closed the connection without replying".into(),
    ));
  }
  serde_json::from_str(&reply).map_err(|e| ControlError::Protocol(format!("bad reply: {e}")))
}

pub fn addr_from_env() -> String {
  std::env::var("SF_ADDR")
    .or_else(|_| std::env::var("SLOPFILE_ADDR"))
    .unwrap_or_else(|_| DEFAULT_ADDR.to_string())
}

pub fn resolve_path(cfg: &AppConfig, session: &str) -> Result<PathBuf, String> {
  let stem = normalize(session)?;
  if stem.contains(".sub-") {
    return Err(format!("no such task `{session}`"));
  }
  Ok(Path::new(&cfg.transcripts_dir).join(format!("{stem}.md")))
}

pub async fn serve(addr: &str, cfg: AppConfig) -> Result<(), String> {
  let listener = TcpListener::bind(addr)
    .await
    .map_err(|e| format!("cannot listen on {addr}: {e}"))?;
  serve_listener(listener, cfg).await
}

pub async fn serve_listener(listener: TcpListener, cfg: AppConfig) -> Result<(), String> {
  match listener.local_addr() {
    Ok(a) => tracing::info!("control channel listening on {a}"),
    Err(e) => tracing::warn!("control channel address unknown: {e}"),
  }
  loop {
    let (stream, _) = match listener.accept().await {
      Ok(pair) => pair,
      Err(e) => {
        tracing::warn!("control accept failed: {e}");
        continue;
      }
    };
    let cfg = cfg.clone();
    tokio::spawn(async move {
      if let Err(e) = serve_conn(stream, &cfg).await {
        tracing::warn!("control request failed: {e}");
      }
    });
  }
}

async fn serve_conn(stream: TcpStream, cfg: &AppConfig) -> Result<(), String> {
  let mut reader = BufReader::new(stream);
  // read_line() grows until it sees a newline, so one peer sending 2 GB without one takes the
  // daemon down. Cap the request and give the peer a deadline.
  let mut line = Vec::new();
  let read = tokio::time::timeout(
    std::time::Duration::from_secs(CONTROL_READ_TIMEOUT_SECS),
    reader.read_until(b'\n', &mut line),
  )
  .await
  .map_err(|_| "control: timed out waiting for a request".to_string())?
  .map_err(|e| format!("read: {e}"))?;
  if read == 0 {
    return Ok(());
  }
  if read > MAX_REQUEST_BYTES {
    return Ok(());
  }
  let line = String::from_utf8_lossy(&line).into_owned();
  let resp = match serde_json::from_str::<Req>(line.trim()) {
    Ok(req) => dispatch(cfg, &req),
    Err(e) => Resp::failed(format!("bad request: {e}")),
  };
  let mut out = serde_json::to_string(&resp)
    .unwrap_or_else(|_| "{\"ok\":false,\"message\":\"reply did not serialize\"}".into());
  out.push('\n');
  let mut stream = reader.into_inner();
  stream
    .write_all(out.as_bytes())
    .await
    .map_err(|e| format!("write: {e}"))?;
  Ok(())
}

fn dispatch(cfg: &AppConfig, req: &Req) -> Resp {
  let name = match normalize(&req.session) {
    Ok(name) => name.to_string(),
    Err(e) => return Resp::failed(e),
  };
  match req.cmd {
    // Arming a file that does not exist can never be swept, so `sf send typo` would report
    // success and exit 0 while doing nothing at all. Every arming command checks first.
    Cmd::Send => match resolve_path(cfg, &req.session) {
      Ok(path) if path.exists() => {
        update(&name, |s| s.arm());
        Resp::done(snapshot(&name).status(&name))
      }
      _ => Resp::failed(format!("no such task `{}`", req.session)),
    },
    Cmd::Stop => match resolve_path(cfg, &req.session) {
      Ok(path) if path.exists() => {
        update(&name, |s| s.disarm());
        Resp::done(snapshot(&name).status(&name))
      }
      _ => Resp::failed(format!("no such task `{}`", req.session)),
    },
    Cmd::Abort => match resolve_path(cfg, &req.session) {
      Ok(path) if path.exists() => {
        update(&name, |s| s.request_abort());
        Resp::done(snapshot(&name).status(&name))
      }
      _ => Resp::failed(format!("no such task `{}`", req.session)),
    },
    Cmd::Status => match resolve_path(cfg, &req.session) {
      Ok(path) if path.exists() => Resp::done(snapshot(&name).status(&name)),
      _ => Resp::failed(format!("no such task `{}`", req.session)),
    },
    Cmd::Question => match answer(cfg, req) {
      Ok(()) => {
        update(&name, |s| s.arm());
        Resp::done(snapshot(&name).status(&name))
      }
      Err(e) => Resp::failed(e),
    },
  }
}

/// The one rule for turning user input into a session name: a bare stem, no path, no surprises.
/// The registry key and the file path are both derived from it, so they can never disagree.
pub fn normalize(session: &str) -> Result<&str, String> {
  let name = session.trim();
  if name.is_empty()
    || name.len() > MAX_SESSION_NAME
    || name.contains('/')
    || name.contains('\\')
    || name.contains("..")
    || name.starts_with('.')
  {
    return Err(format!("no such task `{session}`"));
  }
  Ok(name.strip_suffix(".md").unwrap_or(name))
}

fn answer(cfg: &AppConfig, req: &Req) -> Result<(), String> {
  let path = resolve_path(cfg, &req.session)?;
  if !path.exists() {
    return Err(format!("no such task `{}`", req.session));
  }
  let session = transcript::load(&path)?;
  let pending = session
    .pending_question()
    .ok_or_else(|| format!("no pending questions for task `{}`", req.session))?;
  let answers = questions::resolve_positional(&pending.questions, &req.answers)?;
  let mut session = session;
  session.answer_question(answers)?;
  transcript::save(&path, &session)
}

pub fn create_session(path: &Path, cfg: &AppConfig, excluded: &[String]) -> Result<(), String> {
  if path
    .file_stem()
    .is_some_and(|s| s.to_string_lossy().contains(flow::SUBAGENT_SUFFIX))
  {
    return Err(format!(
      "{}: transcript names must not contain '{}' (reserved for subagents)",
      path.display(),
      flow::SUBAGENT_SUFFIX
    ));
  }
  if path.exists()
    && !transcript::read_file(path)
      .unwrap_or_default()
      .trim()
      .is_empty()
  {
    return Err(format!(
      "{} exists and is not empty; refusing to overwrite",
      path.display()
    ));
  }
  // The system prompt explains the context file to anyone reading the transcript, including the
  // model resuming from it. It is written once and never rewritten after this.
  let session = flow::session_template(path, cfg, excluded);
  transcript::save(path, &session)
}
