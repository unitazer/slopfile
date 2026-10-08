use crate::control::{self, Phase};
use crate::flow;
use crate::session::{self, Session, ToolOutcome};
use crate::stream::StreamWriter;
use crate::tokens;
use crate::transcript;
use crate::api::LlamaClient;
use crate::types::{AppConfig, Confirmations};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

fn gen_lock() -> &'static AsyncMutex<()> {
  static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| AsyncMutex::new(()))
}

fn delivery_lock() -> &'static AsyncMutex<()> {
  static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| AsyncMutex::new(()))
}

/// One `sf send` opens a burst that runs until the model stops calling tools. The cap exists so a
/// wedged model cannot spin forever, but it is a per-burst budget, so a long task has to fit inside
/// it -- which is why hitting it is reported in the transcript rather than silently.
pub const MAX_TURNS_PER_BURST: usize = 20;

pub async fn drive_file(path: &Path, cfg: &AppConfig, client: &LlamaClient) -> Result<(), String> {
  let name = control::session_name(path);
  let is_sub = flow::is_subagent_path(path);
  let eff_cfg = cfg.clone();
  let mut turns = 0;
  let token_at_entry = control::update(&name, |s| s.arm_token());
  loop {
    turns += 1;
    if turns > MAX_TURNS_PER_BURST {
      // Say so in the file. Going quiet looks identical to "the model decided it was done", so
      // the user is left sending `sf send` to find out whether anything is actually wrong.
      let mut s = transcript::load(path).unwrap_or_else(|_| Session::default());
      s.add_note(
        session::NoteBy::Harness,
        format!(
          "[harness] stopped this burst after {MAX_TURNS_PER_BURST} turns without being asked to. \
           Anything you had not finished is still below; continue, but do it in fewer turns — \
           run long jobs in the background instead of polling them, and batch work up."
        ),
      );
      let _ = transcript::save(path, &s);
      warn!("{path:?}: giving up after {turns} turns (noted in the transcript)");
      if is_sub {
        deliver_subagent_result(path, SubagentReport::TurnCap).await?;
      }
      control::update(&name, |s| {
        s.running = false;
        s.settle(token_at_entry);
      });
      return Ok(());
    }

    let mut session = transcript::load(path)?;
    if session.system.trim().is_empty()
      && session.turns.is_empty()
      && !session.has_input()
      && transcript::read_file(path)?.trim().is_empty()
    {
      transcript::save(path, &flow::session_template(cfg, &[]))?;
      info!("{path:?}: initialized empty session");
      return Ok(());
    }
    // While a question is pending the input region belongs to the answer UI, not to a new turn.
    if session.has_input()
      && !session.turn_in_progress()
      && session.pending_question().is_none()
    {
      session.promote_input();
      transcript::save(path, &session)?;
    }

    let pending_subagents = unfinished_subagents(path, &session)?;
    if !pending_subagents.is_empty() {
      info!(
        "{path:?}: waiting on {} subagent(s)",
        pending_subagents.len()
      );
      control::update(&name, |s| s.phase = Phase::WaitingSubagents);
      return Ok(());
    }

    let armed = control::update(&name, |s| {
      s.running = false;
      s.phase = Phase::Idle;
      if s.model.is_empty() {
        s.model = session
          .model()
          .map(str::to_string)
          .unwrap_or_else(|| cfg.model.clone());
      }
      s.pending_questions = session.pending_question().map_or(0, |q| q.questions.len());
      s.armed
    // A subagent is autonomous: nobody is watching it and nobody will ever run `sf send` on it,
    // so waiting for an armed flag would wedge it after one round and hang the parent that is
    // blocked on its report. This is about liveness, not about answering questions for it.
    }) || is_sub || eff_cfg.confirmations == Confirmations::Yolo;

    match next_action(&session, &eff_cfg, armed) {
      Action::Idle => {
        control::update(&name, |s| {
          s.phase = Phase::Idle;
          s.clear_abort();
          s.settle(token_at_entry);
        });
        if is_sub {
          deliver_subagent_result(path, SubagentReport::FromTranscript).await?;
        }
        return Ok(());
      }
      Action::ResolveQuestions => {
        if flow::resolve_questions(&mut session, &eff_cfg)? {
          transcript::save(path, &session)?;
          continue;
        }
        control::update(&name, |s| s.phase = Phase::WaitingAnswer);
        return Ok(());
      }
      Action::WaitForAnswer => {
        control::update(&name, |s| s.phase = Phase::WaitingAnswer);
        return Ok(());
      }
      Action::Generate => {}
    }

    match run_generation_turn(path, session, &eff_cfg, client, &name).await? {
      TurnOutcome::Done => {
        control::update(&name, |s| s.settle(token_at_entry));
        return Ok(());
      }
      TurnOutcome::AwaitsNextTurn => {
        // F7: an abort that lands while the tools are running still ends the burst.
        if control::snapshot(&name).abort {
          control::update(&name, |s| {
            s.phase = Phase::Idle;
            s.clear_abort();
            s.settle(token_at_entry);
          });
          return Ok(());
        }
        continue;
      }
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
  ResolveQuestions,
  Generate,
  WaitForAnswer,
  Idle,
}

pub(crate) fn next_action(session: &Session, cfg: &AppConfig, armed: bool) -> Action {
  if session.pending_question().is_some() {
    return match (armed, flow::questions_resolvable(session, cfg)) {
      (true, true) => Action::ResolveQuestions,
      (true, false) => Action::WaitForAnswer,
      (false, _) => Action::Idle,
    };
  }
  if !armed {
    return Action::Idle;
  }
  if session.needs_generation() {
    return Action::Generate;
  }
  Action::Idle
}

enum TurnOutcome {
  Done,
  AwaitsNextTurn,
}

async fn run_generation_turn(
  path: &Path,
  session: Session,
  cfg: &AppConfig,
  client: &LlamaClient,
  name: &str,
) -> Result<TurnOutcome, String> {
  let mut work = session.clone();
  if work.turn_in_progress() {
    let (reasoning, text) = work.rewind_open_round();
    if let Some(r) = reasoning.as_deref() {
      work.push_reasoning(r);
    }
    if let Some(t) = text.as_deref() {
      work.push_text(t);
    }
  } else {
    work.begin_round();
  }
  let resuming = session.turn_in_progress();
  if resuming {
    transcript::save(path, &work)?;
    info!("{path:?}: resuming a turn left open by an earlier run");
  }

  let model = work
    .model()
    .map(str::to_string)
    .unwrap_or_else(|| cfg.model.clone());

  let thinking = work.thinking();

  // Build the request first, then measure it: the context paragraph is part of what gets sent, so
  // counting before adding it would under-count by a thousand characters.
  let mut messages = work.request_messages();
  // First pass with no fill level: the paragraph must be counted, but the count does not exist yet.
  crate::context::attach_context_file(&mut messages, path, None);
  let plan = tokens::build_request_messages(&work, messages, cfg, client.rest()).await?;
  let mut messages = plan.messages;
  // Second pass with the real number, so the model knows how much room it has before it decides
  // whether to spend a turn compacting. The line itself costs ~40 tokens, which is noise here.
  let budget = tokens::context_budget(&work, cfg);
  crate::context::attach_context_file(
    &mut messages,
    path,
    Some(crate::context::Window { used: plan.token_count, budget }),
  );

  let max_tokens = tokens::output_allowance(tokens::context_budget(&work, cfg));
  let tool_defs = crate::tools::advertised(&work.config);

  client.ensure_model_loaded(&model).await;

  let mut active = StreamWriter::new(path, work);
  info!(
    "→ {path:?}: generating (model={model}, thinking={thinking}, messages={}, tokens={}, dropped={})",
    messages.len(),
    plan.token_count,
    plan.dropped
  );
  // The lock serializes the model, not the tools: holding it across a 30s bash would stall every
  // other session behind it.
  let result = {
    let _gen = gen_lock().lock().await;
    control::update(name, |s| {
      s.running = true;
      s.phase = Phase::Generating;
      s.model = model.clone();
      s.clear_abort();
    });
    client
      .chat_stream(
        &model,
        &messages,
(!tool_defs.is_empty()).then_some(tool_defs.as_slice()),
      max_tokens,
      thinking,
      |ev| active.handle(&ev),
        || !control::update(name, |s| s.take_abort()),
      )
      .await
  }
  .map_err(|e| {
    control::update(name, |s| {
      s.running = false;
      s.phase = Phase::Idle;
    });
    format!("chat_stream: {e}")
  })?;

  active.finish_section();
  let mut streamed = active.into_session();
  control::update(name, |s| {
    s.running = false;
    s.phase = Phase::RunningTools;
  });

  if result.aborted {
    streamed.close_turn();
    transcript::save(path, &streamed)?;
    control::update(name, |s| s.phase = Phase::Idle);
    maybe_restart_llama(cfg, path).await;
    return Ok(TurnOutcome::Done);
  }

  if result.finish_reason.as_deref() == Some("length") {
    flow::add_context_full_note(&mut streamed);
    streamed.close_turn();
    transcript::save(path, &streamed)?;
    control::update(name, |s| {
      s.phase = Phase::Idle;
      s.context_full = true;
    });
    warn!("⚠ {path:?}: context exhausted mid-turn (finish_reason=length)");
    return Ok(TurnOutcome::Done);
  }

  if result.finish_reason.is_none() {
    transcript::save(path, &streamed)?;
    control::update(name, |s| {
      s.running = false;
      s.phase = Phase::Idle;
    });
    warn!("⚠ {path:?}: turn interrupted mid-stream (no finish_reason); left open for resume");
    return Ok(TurnOutcome::Done);
  }

  if !result.tool_calls.is_empty() {
    let calls: Vec<session::Call> = result
      .tool_calls
      .iter()
      .map(|c| session::Call {
        id: c.id.clone(),
        name: c.name.clone(),
        arguments: c.arguments.clone(),
      })
      .collect();
    if let Some(at) = result.tool_calls.iter().position(|c| c.name == "question") {
      let question = &result.tool_calls[at];
      // `question` is not advertised to subagents, so reaching here means the model invented it.
      // Recording a default answer would put a decision in the report that nobody ever made.
      if flow::is_subagent_path(path) {
        let (mut outcomes, _) = run_calls(&streamed, &result.tool_calls[..at], cfg, path).await?;
        outcomes.push(ToolOutcome {
          id: question.id.clone(),
          name: "question".to_string(),
          ok: false,
          ms: 0,
          output: "refused: you cannot ask the user a question. This is a subagent and nobody is \
                   watching it, so there is no one to answer. State the assumption in your final \
                   report instead, or pick a concrete option and say which one and why."
            .to_string(),
        });
        streamed = settle_tool_round(streamed, calls, &mut outcomes, path);
        transcript::save(path, &streamed)?;
        return Ok(TurnOutcome::Done);
      }
      // Calls that shared the message with the question still have to happen, or the model is
      // later shown an answer with no record that it ever asked for them.
      // Calls that shared the message with the question still run; the question itself is recorded
      // as a result that says it is waiting, and any call left over gets an honest placeholder.
      let before = &result.tool_calls[..at];
      let (mut outcomes, solved) = run_calls(&streamed, before, cfg, path).await?;
      if solved {
        control::update(name, |s| {
          s.solved = true;
          s.phase = Phase::Idle;
        });
        info!("✓ {path:?}: campaign marked solved");
      }
      streamed = settle_tool_round(streamed, calls, &mut outcomes, path);
      flow::render_question(&mut streamed, question)?;
      transcript::save(path, &streamed)?;
      return Ok(TurnOutcome::AwaitsNextTurn);
    }
    let (mut outcomes, solved) = run_calls(&streamed, &result.tool_calls, cfg, path).await?;
    streamed = settle_tool_round(streamed, calls, &mut outcomes, path);
    transcript::save(path, &streamed)?;
    if solved {
      control::update(name, |s| {
        s.solved = true;
        s.phase = Phase::Idle;
      });
      streamed.close_turn();
      transcript::save(path, &streamed)?;
      info!("✓ {path:?}: campaign marked solved");
      return Ok(TurnOutcome::Done);
    }
    info!("→ {path:?}: {} tool call(s) executed", outcomes.len());
    return Ok(TurnOutcome::AwaitsNextTurn);
  }

  streamed.close_turn();
  transcript::save(path, &streamed)?;
  control::update(name, |s| s.phase = Phase::Idle);
  info!("✓ {path:?}: turn complete");
  Ok(TurnOutcome::Done)
}

pub(crate) async fn run_calls(
  session: &Session,
  calls: &[crate::types::ToolCall],
  cfg: &AppConfig,
  path: &Path,
) -> Result<(Vec<session::ToolOutcome>, bool), String> {
  if calls.is_empty() {
    return Ok((Vec::new(), false));
  }
  match flow::execute_tool_calls(calls, session, cfg, path).await? {
    flow::ToolPass::Outcomes { outcomes, solved } => Ok((outcomes, solved)),
    flow::ToolPass::Question => Ok((Vec::new(), false)),
  }
}

pub(crate) fn settle_tool_round(
  mut streamed: Session,
  calls: Vec<crate::session::Call>,
  outcomes: &mut Vec<ToolOutcome>,
  path: &Path,
) -> Session {
  match flow::adopt_model_edit(&streamed, path) {
    Ok(Some(mut edited)) => {
      if edited.turns.last().and_then(|t| t.rounds.last()).is_none() {
        edited.begin_round();
      }
      edited.finish_round(calls, outcomes.clone());
      edited
    }
    Ok(None) => {
      streamed.finish_round(calls, outcomes.clone());
      streamed
    }
    Err(why) => {
      warn!("⚠ {}: context edit rolled back: {why}", path.display());
      flow::note_rollback(outcomes, &why);
      streamed.finish_round(calls, outcomes.clone());
      streamed
    }
  }
}

async fn maybe_restart_llama(cfg: &AppConfig, path: &Path) {
  if !cfg.abort_restart_server {
    info!("✗ {path:?}: aborted");
    return;
  }
  let container = cfg.llama_container.clone();
  let res = tokio::task::spawn_blocking(move || {
    crate::docker::restart_llama_server("/var/run/docker.sock", &container)
  })
  .await;
  match res {
    Ok(Ok(())) => info!("✗ {path:?}: aborted (llama-server restarted)"),
    Ok(Err(e)) => warn!("{path:?}: aborted but couldn't restart llama-server: {e}"),
    Err(e) => warn!("{path:?}: aborted, llama restart task failed: {e}"),
  }
}

fn final_assistant_text(session: &Session) -> Option<String> {
  session
    .turns
    .iter()
    .rev()
    .flat_map(|t| t.rounds.iter().rev())
    .find_map(|r| {
      r.assistant
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
    })
}

fn subagent_parent_path(child: &Path) -> Result<PathBuf, String> {
  let dir = child.parent().ok_or("subagent transcript has no directory")?;
  let stem = child
    .file_stem()
    .ok_or("subagent transcript has no file stem")?
    .to_string_lossy()
    .into_owned();
  let ix = stem.rfind(flow::SUBAGENT_SUFFIX).ok_or("not a subagent transcript")?;
  Ok(dir.join(format!("{}.md", &stem[..ix])))
}

pub fn child_paths(parent: &Path) -> Result<Vec<PathBuf>, String> {
  let dir = parent.parent().ok_or("parent transcript has no directory")?;
  let stem = parent
    .file_stem()
    .ok_or("parent transcript has no file stem")?
    .to_string_lossy()
    .into_owned();
  let prefix = format!("{stem}{}", flow::SUBAGENT_SUFFIX);
  let mut out = Vec::new();
  for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
    let p = match entry {
      Ok(e) => e.path(),
      Err(_) => continue,
    };
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
      continue;
    };
    if !name.starts_with(&prefix) || !name.ends_with(".md") {
      continue;
    }
    let suffix = name
      .trim_end_matches(".md")
      .strip_prefix(&prefix)
      .expect("name starts with prefix");
    if suffix.len() != 2 || !suffix.bytes().all(|b| b.is_ascii_digit()) {
      continue;
    }
    out.push(p);
  }
  out.sort();
  Ok(out)
}

pub(crate) fn unfinished_subagents(parent: &Path, parent_session: &Session) -> Result<Vec<PathBuf>, String> {
  Ok(
    child_paths(parent)?
      .into_iter()
      .filter(|child| {
        let stem = child.file_stem().unwrap_or_default().to_string_lossy();
        let short = stem
          .rsplit(flow::SUBAGENT_SUFFIX)
          .next()
          .unwrap_or_default()
          .to_string();
        // Either proof counts: the report is in the parent, or we already delivered it. Trusting
        // only the file would wedge the parent forever if the model prunes the report turn, which
        // the context prompt explicitly invites it to do.
        !(flow::has_report(parent_session, &short)
          || control::snapshot(&control::session_name(child)).delivered)
      })
      .collect(),
  )
}

pub(crate) enum SubagentReport {
  FromTranscript,
  TurnCap,
}

pub(crate) async fn deliver_subagent_result(
  path: &Path,
  outcome: SubagentReport,
) -> Result<(), String> {
  let name = control::session_name(path);
  if control::snapshot(&name).delivered {
    return Ok(());
  }
  // The parent is read-modify-written, so two children finishing at the same moment would lose one
  // of the reports. Delivery is rare, so one lock for all of it is enough.
  let _delivery = delivery_lock().lock().await;
  if control::snapshot(&name).delivered {
    return Ok(());
  }
  let parent_path = subagent_parent_path(path)?;
  let mut parent = transcript::load(&parent_path)?;
  let session = transcript::load(path).unwrap_or_default();
  let short = path
    .file_stem()
    .map(|s| s.to_string_lossy().into_owned())
    .unwrap_or_else(|| "subagent".to_string());
  let short = short
    .rsplit(flow::SUBAGENT_SUFFIX)
    .next()
    .unwrap_or_default()
    .to_string();
  let (report, ok) = match outcome {
    SubagentReport::FromTranscript => (
      final_assistant_text(&session)
        .unwrap_or_else(|| "(subagent produced no text output)".to_string()),
      true,
    ),
    SubagentReport::TurnCap => {
      let body = final_assistant_text(&session)
        .unwrap_or_else(|| "(subagent produced no text output)".to_string());
      (
        format!(
          "(the subagent hit the per-burst turn cap and was cut off mid-work; the report below may be incomplete)\n\n{body}"
        ),
        true,
      )
    }
  };
  let injected = flow::inject_subagent_report(&mut parent, &short, &report, ok);
  transcript::save(&parent_path, &parent)?;
  // The parent may be mid-generation in another task, and that task's next save would drop this
  // turn. Read back before latching `delivered`, so a lost report is retried instead of forgotten.
  let survived = transcript::load(&parent_path)
    .map(|after| flow::has_report(&after, &short))
    .unwrap_or(false);
  control::update(&name, |s| s.delivered = survived);
  if injected && !survived {
    return Err(format!(
      "the report for {short} was written to {} but did not stick; it will be retried",
      parent_path.display()
    ));
  }
  if !injected {
    warn!(
      "→ {:?}: report for {short} was already in the parent, not injecting again",
      path
    );
    return Ok(());
  }
  info!("→ {:?}: report delivered to parent", path);
  Ok(())
}
