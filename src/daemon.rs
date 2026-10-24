use crate::api::{ApiError, LlamaClient};
use crate::control::{self, Phase};
use crate::flow;
use crate::session::{self, Session, ToolOutcome};
use crate::stream::StreamWriter;
use crate::tokens;
use crate::transcript;
use crate::types::{AppConfig, Confirmations, Endpoint, PromptSample};
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

/// Write why a request did not happen into the transcript itself.
///
/// The failure used to exist only in the daemon log, which left the file looking exactly like a
/// session that simply finished -- same trailing empty input region, same silence -- so the only
/// way to find out was `docker logs` after the fact. A provider refusal is the one failure a user
/// is most likely to hit and least likely to have any other way of diagnosing.
pub(crate) fn record_provider_failure(
  path: &Path,
  mut s: Session,
  e: &ApiError,
) -> Result<(), String> {
  // This runs on a transcript that may be mid-stream, with `text` still open so a later run can
  // resume mid-sentence. Notes render inside the open region, and the parser then reads the note
  // *and the whole input region* as part of the model's partial answer -- after which the file
  // no longer parses at all and every later run dies on load. Close the dangling section first;
  // the turn stays open, so the partial answer is still there and still resumable.
  s.close_section();
  // The session we were handed is the snapshot taken before the request went out, but the human
  // has had the whole generation to edit the file -- pruning turns, fixing a typo, typing the
  // next message. Saving the snapshot wholesale silently reverts all of it, which is worst for the
  // overflow case: the note tells them to shrink this transcript, they do it mid-request, and it
  // comes straight back.
  //
  // So the file wins, and only the one thing it cannot know about is put back: whatever the model
  // streamed into the turn that is still in flight.
  match transcript::load(path) {
    Ok(on_disk) => {
      let in_flight = s
        .turns
        .last()
        .and_then(|t| t.rounds.last())
        .map(|r| r.assistant.clone());
      s = on_disk;
      if let Some(partial) = in_flight
        && let Some(round) = s.turns.last_mut().and_then(|t| t.rounds.last_mut())
      {
        // Only graft onto the round this fragment actually belongs to. Matching by position is not
        // enough: if the user prunes the dangling assistant region mid-generation, the last round
        // on disk is some earlier one, and writing a fragment of the aborted reply over its text
        // would leave the transcript claiming the model said something it never said.
        //
        // Two things identify the real target. An unanswered round has no calls and no results --
        // though a plain prose answer satisfies that too, so it is not enough on its own. The text
        // is the real tell: the in-flight round on disk holds what has flushed so far, which is a
        // prefix of what we hold. A completed round's text is something else entirely.
        let unanswered = round.assistant.calls.is_empty() && round.results.is_empty();
        let same_round = match (round.assistant.text.as_deref(), partial.text.as_deref()) {
          (Some(on_disk), Some(streamed)) => on_disk.is_empty() || streamed.starts_with(on_disk),
          _ => true,
        };
        if unanswered && same_round {
          if partial.reasoning.is_some() {
            round.assistant.reasoning = partial.reasoning;
          }
          if partial.text.is_some() {
            round.assistant.text = partial.text;
          }
        }
      }
    }
    // Falling back to the pre-request snapshot here would quietly undo every edit the human made
    // while the request was running -- the one thing this function exists to prevent -- so it is
    // worth saying out loud rather than losing their words without a word.
    Err(e) => tracing::warn!(
      "{path:?}: could not re-read the transcript to preserve your edits ({e});      writing the in-flight state over it"
    ),
  }
  // Only a refusal is the provider saying no. A timeout or a dropped connection is a different
  // thing entirely, and promising "it will come back on the next send" there is advice that can be
  // wrong -- a stream that died mid-generation may have lost the turn it was writing.
  // A 408, a 425, or anything with `Retry-After` is a refusal that expires, not a dropped
  // connection. Describing it as the latter sends the user looking at their network instead of at
  // the rate limit that just hit them.
  let is_refusal = matches!(e, ApiError::NonOkStatus(..));
  let note = if e.is_context_overflow() {
    // The single most likely first-turn failure on a hosted endpoint, and the one case where
    // "re-sending is safe" is exactly wrong: the request is too big, so it fails identically until
    // the transcript shrinks.
    format!(
      "[harness] {e}\n   The provider rejected this because the request was too large for the \
       model's context window, not because of anything in it. Re-sending will fail the same way \
       until the transcript shrinks: delete or compress older turns in this file, or raise \
       `max_context_tokens` if the model really does have a bigger window."
    )
  } else if e.is_terminal() {
    format!(
      "[harness] {e}\n   The provider refused the request, so this turn was never answered. \
       Nothing in the transcript was lost and re-sending is safe."
    )
  } else {
    if is_refusal {
      format!(
        "[harness] {e}\n   The provider turned this one down but said it is temporary, so it will \
         clear on its own. Re-sending later is safe; check whether the turn above was left \
         half-written."
      )
    } else {
      format!(
        "[harness] {e}\n   The connection failed rather than being refused, so this is likely \
         transient. Re-sending is safe, but check whether the turn above was left half-written."
      )
    }
  };
  // A send against the same dead endpoint fails the same way, and a note per attempt would bury the
  // transcript it is trying to explain. Replace the previous one instead of stacking.
  if let Some(last) = s.turns.last_mut()
    && let Some(note_slot) = last.notes.last_mut()
    && note_slot.by == session::NoteBy::Harness
    && (note_slot.text.contains("this turn was never answered")
      || note_slot.text.contains("left half-written")
      || note_slot.text.contains("too large for the"))
  {
    note_slot.text = note;
  } else {
    s.add_note(session::NoteBy::Harness, &note);
  }
  transcript::save(path, &s)
}

/// The text a burst writes when it hits a ceiling. Kept as one function so the test asserts on
/// what is actually emitted rather than on a copy that can drift.
pub(crate) fn burst_ceiling_note(burst_cap: u32) -> String {
  format!(
    "[harness] this burst hit its turn ceiling ({burst_cap}). There is no ceiling by default — \
     set `burst_max_turns = 0` in the fence above (or in `shared/slopfile.toml`) to make that \
     explicit and unlimited; without a fence value, the app-wide setting applies. The work is all \
     above and continues from there."
  )
}

/// The answer the model actually showed, with inline think tags removed -- the same split the
/// transcript writer makes, so "did it say anything visible?" is asked about the same text the user
/// would see.
pub(crate) fn visible_text(content: &str) -> String {
  // Alternates on both tags, like the transcript writer does. A single `<think>` pass loses a
  // second think block, and a truncated reply is exactly the shape with more than one -- which
  // would report visible output that was only thinking and misdiagnose the turn as context-full.
  let mut out = String::new();
  let mut rest = content;
  let mut in_think = false;
  while let Some(i) = rest
    .find("<think>")
    .into_iter()
    .chain(rest.find("</think>"))
    .min()
  {
    if !in_think {
      out.push_str(&rest[..i]);
    }
    if rest[i..].starts_with("<think>") {
      rest = &rest[i + "<think>".len()..];
      in_think = true;
    } else {
      rest = &rest[i + "</think>".len()..];
      in_think = false;
    }
  }
  if !in_think {
    out.push_str(rest);
  }
  out
}

/// A turn that never left the building -- a misspelled provider, unusable headers -- rather than
/// one the provider turned down. Same job as `record_provider_failure`: the reason belongs in the
/// file, because the file otherwise just stops for no visible reason.
pub(crate) fn record_config_failure(path: &Path, e: &str) -> Result<(), String> {
  let mut s = transcript::load(path)?;
  s.close_section();
  let note = format!(
    "[harness] {e}\n   Nothing was sent. If the provider is named in this file's fence, fix it \
     there; otherwise it is the app-wide `provider` in `shared/slopfile.toml`, or a \
     `SLOPFILE_*` environment variable. Re-sending alone will fail the same way."
  );
  if let Some(last) = s.turns.last_mut()
    && let Some(note_slot) = last.notes.last_mut()
    && note_slot.by == session::NoteBy::Harness
    && note_slot.text.contains("Nothing was sent")
  {
    note_slot.text = note;
  } else {
    s.add_note(session::NoteBy::Harness, &note);
  }
  transcript::save(path, &s)
}

/// One `sf send` opens a burst that runs until the model stops calling tools.
///
/// There is deliberately no turn limit. There used to be one at 20, and it was a band-aid: a bug
/// in `bash` made a correctly-backgrounded job inherit the pipe, so the call hung to timeout and
/// then killed the job it had just launched, and the model responded the only way it could -- by
/// polling in 28-second sleeps. Every poll was a turn, so a 23-call session spent its whole budget
/// watching a job that had already finished. The bug is fixed (2d56f21); the scar was not, and by
/// then a turn was worth ~5s on a hosted model, so the cap was stopping legitimate work.
///
/// `AppConfig::burst_max_turns` is still there for anyone who wants a ceiling -- it defaults to
/// 0, meaning none -- and `sf abort` / `sf stop` remain the real control.
pub async fn drive_file(path: &Path, cfg: &AppConfig, client: &LlamaClient) -> Result<(), String> {
  let name = control::session_name(path);
  let is_sub = flow::is_subagent_path(path);
  let eff_cfg = cfg.clone();
  let mut turns = 0;
  let token_at_entry = control::update(&name, |s| s.arm_token());
  loop {
    turns += 1;

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
    // Opt-in ceiling, off unless asked for. Checked here rather than at the top of the loop so a
    // session can set its own -- the fence is the per-session dial for everything else.
    let burst_cap = session
      .config
      .burst_max_turns
      .unwrap_or(cfg.burst_max_turns);
    if burst_cap > 0 && turns as u32 > burst_cap {
      // Say so in the file. Going quiet looks identical to "the model decided it was done", so
      // the user is left sending `sf send` to find out whether anything is actually wrong.
      session.add_note(session::NoteBy::Harness, burst_ceiling_note(burst_cap));
      let _ = transcript::save(path, &session);
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
    // While a question is pending the input region belongs to the answer UI, not to a new turn.
    if session.has_input() && !session.turn_in_progress() && session.pending_question().is_none() {
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
          .unwrap_or_else(|| cfg.default_session_model.clone());
      }
      s.pending_questions = session.pending_question().map_or(0, |q| q.questions.len());
      s.armed
      // A subagent is autonomous: nobody is watching it and nobody will ever run `sf send` on it,
      // so waiting for an armed flag would wedge it after one round and hang the parent that is
      // blocked on its report. This is about liveness, not about answering questions for it.
    }) || is_sub
      || eff_cfg.confirmations == Confirmations::Yolo;

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
    .unwrap_or_else(|| cfg.default_session_model.clone());

  let thinking = work.thinking();

  // Re-resolved every turn, not once at startup: the fence is the user's dial and they can turn it
  // between turns. `Err` here is a typo'd provider name, and it must stop the turn rather than
  // quietly falling back to some other endpoint's model.
  //
  // It also gets written into the transcript. A misspelled provider leaves the file ending in an
  // empty input region, which is exactly the silence the failure note exists to end -- and this
  // one is entirely the user's typo, so it is the case they are most likely to hit and least
  // likely to guess at.
  let client = match client.for_provider(cfg, work.provider(), work.headers()) {
    Ok(c) => c,
    Err(e) => {
      record_config_failure(path, &e)?;
      return Err(e);
    }
  };
  let client = &client;
  let endpoint = client.endpoint().base.clone();

  // Build the request first, then measure it: the context paragraph is part of what gets sent, so
  // counting before adding it would under-count by a thousand characters.
  let mut messages = work.request_messages();
  // First pass with no fill level: the paragraph must be counted, but the count does not exist yet.
  crate::context::attach_context_file(&mut messages, path, None);
  let tool_defs = crate::tools::advertised(&work.config);
  let tools_chars = serde_json::to_string(&tool_defs).map_or(0, |s| s.len() as u64);
  let anchor = control::update(name, |s| s.last_prompt.clone());
  let plan = tokens::build_request_messages(
    &work,
    messages,
    cfg,
    client.endpoint(),
    client,
    &tokens::Counting {
      anchor,
      tools_chars,
    },
  )
  .await?;
  // The size of the context this request was built from -- i.e. before the calls it makes. That is
  // the reading the model can act on, and it is the only one that exists: `usage` for this request
  // has not come back yet. `None` on the first turn against a new endpoint, where there is nothing
  // measured to report, and then the results carry no tag at all.
  let tell_ctx = cfg.tell_context_size(&work.config);
  let ctx_tokens = plan.token_count;
  let mut messages = plan.messages;
  // Second pass with the real number, so the model knows how much room it has before it decides
  // whether to spend a turn compacting. The line itself costs ~40 tokens, which is noise here.
  // With no measured sample there is no honest number to give, so the line stays off entirely
  // rather than the model being told a figure we made up.
  let budget = tokens::context_budget(&work, cfg);
  crate::context::attach_context_file(
    &mut messages,
    path,
    plan.token_count.map(|used| crate::context::Window {
      used,
      // The prompt competes for the window minus the reply's allowance, so that -- not the
      // whole window -- is what "how full am I" is measured against.
      usable: tokens::prompt_budget(budget),
      total: budget,
    }),
  );

  let max_tokens = tokens::output_allowance(tokens::context_budget(&work, cfg));

  client.ensure_model_loaded(&model).await;

  let mut active = StreamWriter::new(path, work);
  info!(
    "→ {path:?}: generating (provider={}, model={model}, thinking={thinking}, messages={}, tokens={}, dropped={})",
    endpoint,
    messages.len(),
    plan.token_count.map_or("?".to_string(), |n| n.to_string()),
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
      s.endpoint = endpoint.clone();
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
  };
  let result = match result {
    Ok(result) => result,
    Err(e) => {
      control::update(name, |s| {
        s.running = false;
        s.phase = Phase::Idle;
      });
      // Leave it in the file. A provider that refuses the request and a model that has finished
      // look identical from the outside -- both leave the transcript ending in an empty input
      // region -- so without this the only place the reason existed was a log nobody reads.
      record_provider_failure(path, active.into_session(), &e)?;
      if e.is_terminal() {
        // Retrying a refusal just spends more of whatever the provider is limiting. A daily quota
        // does not come back in five seconds, and a turn that still owes a reply means every
        // sweep would otherwise try again -- forty requests in three seconds, measured.
        control::update(name, |s| s.blocked = true);
        warn!("{path:?}: {e}; parked until you send again");
        if flow::is_subagent_path(path) {
          deliver_subagent_result(path, SubagentReport::ProviderRefusal(e.to_string())).await?;
        }
        return Ok(TurnOutcome::Done);
      }
      return Err(format!("chat_stream: {e}"));
    }
  };

  // Pair what we sent with what the provider counted, so the next turn has a measured density
  // instead of an assumed one. Recorded from the list that actually went out, after truncation.
  if let Some(usage) = &result.usage
    && usage.prompt_tokens > 0
  {
    let sample = PromptSample {
      chars: plan.payload_chars,
      tokens: usage.prompt_tokens,
      endpoint: client.endpoint().base.clone(),
      model: model.clone(),
    };
    control::update(name, |s| s.last_prompt = Some(sample));
  }

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
    maybe_restart_llama(cfg, client.endpoint(), path).await;
    return Ok(TurnOutcome::Done);
  }

  if result.finish_reason.as_deref() == Some("length") {
    // `length` means the output allowance ran out, which is not the same as the context window
    // filling up. When reasoning tokens are billed out of that same allowance, a turn can stop
    // with nothing visible written at all -- and telling the model to prune its transcript would
    // be advice that cannot possibly help. `context_full` stays false here for the same reason:
    // the output allowance ran out, not the context window.
    let by_reasoning = result
      .usage
      .as_ref()
      // The visible text, not `result.content`: that is the raw stream including any inline
      // `<think>...</think>` markup, which the writer splits out into the reasoning region. Ask it
      // about text that was really only thinking and the turn gets blamed on context that was
      // never full.
      .is_some_and(|u| u.truncated_by_reasoning(&visible_text(&result.content)))
      // Tool-call arguments never reach `result.content` -- they are accumulated separately. A turn
      // that thought for 25k tokens and then ran out mid-tool-call would otherwise look identical
      // to one that said nothing at all, and be told the transcript is fine when the allowance was
      // the problem.
      && result.tool_calls.is_empty();
    if by_reasoning {
      flow::add_reasoning_exhausted_note(&mut streamed);
      streamed.close_turn();
      transcript::save(path, &streamed)?;
      control::update(name, |s| s.phase = Phase::Idle);
      warn!(
        "⚠ {path:?}: thinking consumed the whole output allowance (finish_reason=length); the \
         transcript was not the problem"
      );
      return Ok(TurnOutcome::Done);
    }
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
        let mut outcomes = run_calls(&streamed, &result.tool_calls[..at], cfg, path).await?;
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
        if tell_ctx {
          flow::annotate_context_size(&mut outcomes, ctx_tokens);
        }
        streamed = settle_tool_round(streamed, calls, &mut outcomes, path);
        transcript::save(path, &streamed)?;
        return Ok(TurnOutcome::Done);
      }
      // Calls that shared the message with the question still have to happen, or the model is
      // later shown an answer with no record that it ever asked for them.
      // Calls that shared the message with the question still run; the question itself is recorded
      // as a result that says it is waiting, and any call left over gets an honest placeholder.
      let before = &result.tool_calls[..at];
      let mut outcomes = run_calls(&streamed, before, cfg, path).await?;
      if tell_ctx {
        flow::annotate_context_size(&mut outcomes, ctx_tokens);
      }
      streamed = settle_tool_round(streamed, calls, &mut outcomes, path);
      flow::render_question(&mut streamed, question)?;
      transcript::save(path, &streamed)?;
      return Ok(TurnOutcome::AwaitsNextTurn);
    }
    let mut outcomes = run_calls(&streamed, &result.tool_calls, cfg, path).await?;
    if tell_ctx {
      flow::annotate_context_size(&mut outcomes, ctx_tokens);
    }
    streamed = settle_tool_round(streamed, calls, &mut outcomes, path);
    transcript::save(path, &streamed)?;
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
) -> Result<Vec<session::ToolOutcome>, String> {
  if calls.is_empty() {
    return Ok(Vec::new());
  }
  flow::execute_tool_calls(calls, session, cfg, path).await
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

async fn maybe_restart_llama(cfg: &AppConfig, endpoint: &Endpoint, path: &Path) {
  // There is only one llama-server, and it is the one that was interrupted. A remote endpoint
  // keeps serving the next request on its own, and its container is not ours to restart.
  if !cfg.abort_restart_server || !endpoint.llama {
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
  let dir = child
    .parent()
    .ok_or("subagent transcript has no directory")?;
  let stem = child
    .file_stem()
    .ok_or("subagent transcript has no file stem")?
    .to_string_lossy()
    .into_owned();
  let ix = stem
    .rfind(flow::SUBAGENT_SUFFIX)
    .ok_or("not a subagent transcript")?;
  Ok(dir.join(format!("{}.md", &stem[..ix])))
}

pub fn child_paths(parent: &Path) -> Result<Vec<PathBuf>, String> {
  let dir = parent
    .parent()
    .ok_or("parent transcript has no directory")?;
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

pub(crate) fn unfinished_subagents(
  parent: &Path,
  parent_session: &Session,
) -> Result<Vec<PathBuf>, String> {
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
  /// The provider refused this subagent's turn. Without a report the parent waits on a child that
  /// will never report, so it parks in WaitingSubagents until someone intervenes by hand.
  ProviderRefusal(String),
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
          "(the subagent hit a per-burst turn ceiling and was cut off mid-work; the report below may be incomplete)\n\n{body}"
        ),
        true,
      )
    }
    SubagentReport::ProviderRefusal(why) => (
      format!(
        "(this subagent could not finish: the provider refused its request, so nothing was \
         gathered. {why}\n\nEither retry it or do the work here.)"
      ),
      false,
    ),
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
