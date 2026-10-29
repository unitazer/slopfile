use std::{env::temp_dir, fs::read_to_string, path::Path, thread::spawn};

use serde_json::json;

use super::*;
use crate::control::{self, Cmd, Req, SessionState};
use crate::daemon::{Action, next_action, run_calls, settle_tool_round};
use crate::docker::restart_llama_server;
use crate::flow::{
  execute_tool_calls, has_report, questions_resolvable, resolve_questions, session_template,
  spawn_subagent, subagent_depth,
};
use crate::session::{self, Session};
use crate::stream::StreamWriter;
use crate::types::{AppConfig, ChatMessage, Confirmations, Role, StreamEvent, ToolCall};
use crate::{flow, tokens, tools, transcript};

fn yolo_cfg() -> AppConfig {
  AppConfig {
    confirmations: Confirmations::Yolo,
    api_base: "http://127.0.0.1:8080".into(),
    providers: Default::default(),
    provider: None,
    burst_max_turns: 0,
    tell_model_context_size: true,
    workdir: "/tmp".into(),
    transcripts_dir: "/tmp".into(),
    skills_dir: "/tmp/does-not-exist".into(),
    sweep_interval_ms: 250,
    default_session_model: "qwen3.8-27b".into(),
    max_context_tokens: 8192,
    tool_timeout_ms: 30000,
    abort_restart_server: false,
    llama_container: "test-llama".into(),
    subagent_max_depth: 2,
  }
}

/// Tests that do not care where the transcript lives still use the real constructor, including the
/// context paragraph, by naming a fixed fake path.
fn template(cfg: &AppConfig, excluded: &[String]) -> Session {
  session_template(Path::new("/t/1.md"), cfg, excluded)
}

fn scratch(tag: &str) -> std::path::PathBuf {
  let dir = temp_dir().join(format!("slopfile-test-{tag}"));
  std::fs::remove_dir_all(&dir).ok();
  std::fs::create_dir_all(&dir).expect("scratch dir");
  dir
}

fn write_session(path: &Path, session: &Session) {
  transcript::save(path, session).expect("save session");
}

fn load(path: &Path) -> Session {
  transcript::load(path).expect("load")
}

fn with_question(input_tail: &str) -> Session {
  let mut session = template(&yolo_cfg(), &[]);
  session.turns.push(session::Turn {
    custom: Vec::new(),
    user: Some("pick one".to_string()),
    notes: vec![],
    rounds: vec![session::Round {
      notes: Vec::new(),
      assistant: session::Assistant {
        custom: Vec::new(),
        reasoning: None,
        text: Some("which?".to_string()),
        calls: vec![],
      },
      results: vec![],
    }],
    question: None,
  });
  session.input = format!(
    "{}{input_tail}",
    questions::render_block(&[questions::Question {
      id: 1,
      question: "red or blue?".into(),
      answers: vec!["red".into(), "blue".into()],
      default: None,
    }])
  );
  session.set_question(session::PendingQuestion {
    call: session::Call {
      id: "q1".into(),
      name: "question".into(),
      arguments: r#"{"data":[]}"#.into(),
    },
    questions: vec![questions::Question {
      id: 1,
      question: "red or blue?".into(),
      answers: vec!["red".into(), "blue".into()],
      default: None,
    }],
  });
  session
}

fn last_results(session: &Session) -> &[session::ToolOutcome] {
  &session
    .turns
    .last()
    .and_then(|t| t.rounds.last())
    .expect("a round")
    .results
}

fn last_text(session: &Session) -> Option<&str> {
  session
    .turns
    .last()
    .and_then(|t| t.rounds.last())
    .and_then(|r| r.assistant.text.as_deref())
}

async fn tool_round(path: &Path, cfg: &AppConfig, calls: &[ToolCall]) -> Session {
  let mut session = load(path);
  if !session.turn_in_progress() {
    session.begin_round();
  }
  session.push_text("working on it");
  write_session(path, &session);
  let mut outcomes = execute_tool_calls(calls, &session, cfg, path)
    .await
    .expect("tools run");
  let calls: Vec<session::Call> = calls
    .iter()
    .map(|c| session::Call {
      id: c.id.clone(),
      name: c.name.clone(),
      arguments: c.arguments.clone(),
    })
    .collect();
  let settled = settle_tool_round(session, calls, &mut outcomes, path);
  write_session(path, &settled);
  settled
}

#[test]
fn docker_restart_speaks_the_docker_api_over_a_unix_socket() {
  use std::io::{Read, Write};
  use std::os::unix::net::UnixListener;

  let dir = scratch("dockersock");
  let sock = dir.join("docker.sock");
  let listener = UnixListener::bind(&sock).unwrap();
  let server = spawn(move || {
    let (mut c, _) = listener.accept().unwrap();
    let mut req = String::new();
    let mut buf = [0u8; 4096];
    loop {
      match c.read(&mut buf) {
        Ok(0) => break,
        Ok(n) => {
          req.push_str(&String::from_utf8_lossy(&buf[..n]));
          if req.contains("\r\n\r\n") {
            break;
          }
        }
        Err(_) => break,
      }
    }
    assert!(
      req.starts_with("POST /containers/clanker-llama/restart HTTP/1.1\r\n"),
      "request line must target the docker restart endpoint: {req}"
    );
    c.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
      .unwrap();
  });

  assert!(
    restart_llama_server(sock.to_str().unwrap(), "clanker-llama").is_ok(),
    "204 -> ok"
  );
  server.join().unwrap();

  let sock2 = dir.join("docker2.sock");
  let listener2 = UnixListener::bind(&sock2).unwrap();
  let path2 = sock2.clone();
  let server2 = spawn(move || {
    let (mut c, _) = listener2.accept().unwrap();
    let mut req = String::new();
    let mut buf = [0u8; 4096];
    loop {
      match c.read(&mut buf) {
        Ok(0) => break,
        Ok(n) => {
          req.push_str(&String::from_utf8_lossy(&buf[..n]));
          if req.contains("\r\n\r\n") {
            break;
          }
        }
        Err(_) => break,
      }
    }
    assert!(req.starts_with("POST /containers/ghost/restart"));
    c.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
      .unwrap();
  });
  let err = restart_llama_server(path2.to_str().unwrap(), "ghost").unwrap_err();
  assert!(
    err.contains("404"),
    "error must surface the status line: {err}"
  );
  server2.join().unwrap();
  std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn yolo_resolves_questions_with_the_first_option() {
  let dir = scratch("yolo-q");
  let path = dir.join("yolo-q.md");
  write_session(&path, &with_question(""));
  let mut session = load(&path);
  let cfg = yolo_cfg();
  assert!(questions_resolvable(&session, &cfg), "yolo always resolves");
  resolve_questions(&mut session, &cfg).expect("resolve");
  write_session(&path, &session);

  let after = load(&path);
  assert!(
    after.input.is_empty(),
    "answer block consumed: {:?}",
    after.input
  );
  assert!(after.pending_question().is_none());
  let messages = after.request_messages();
  assert_eq!(messages.last().unwrap().role, Role::Tool);
  assert!(
    messages
      .last()
      .unwrap()
      .content
      .as_deref()
      .unwrap()
      .contains("\"answer\":\"red\""),
    "first option used: {messages:?}"
  );
  assert!(
    next_action(&after, &cfg, true) == Action::Generate,
    "the daemon continues after a yolo answer"
  );
}

#[test]
fn ask_mode_needs_an_answer_before_it_resolves() {
  let dir = scratch("ask-q");
  let path = dir.join("ask-q.md");
  write_session(&path, &with_question(""));
  let session = load(&path);
  let cfg = AppConfig {
    confirmations: Confirmations::Ask,
    ..yolo_cfg()
  };
  assert!(!questions_resolvable(&session, &cfg));
  assert_eq!(next_action(&session, &cfg, true), Action::WaitForAnswer);
  assert_eq!(
    next_action(&session, &cfg, false),
    Action::Idle,
    "an unarmed session does not even wait"
  );
  let mut answered = session.clone();
  answered.input.push_str("> 2\n");
  assert!(questions_resolvable(&answered, &cfg));
  resolve_questions(&mut answered, &cfg).expect("resolve");
  assert!(
    answered
      .request_messages()
      .last()
      .unwrap()
      .content
      .as_deref()
      .unwrap()
      .contains("\"answer\":\"blue\""),
    "a typed index picks the second option"
  );
}

#[test]
fn skip_resolves_with_defaults() {
  let dir = scratch("skip-q");
  let path = dir.join("skip-q.md");
  let mut session = with_question("");
  session.set_question(session::PendingQuestion {
    call: session::Call {
      id: "q1".into(),
      name: "question".into(),
      arguments: "{}".into(),
    },
    questions: vec![questions::Question {
      id: 1,
      question: "how long?".into(),
      answers: vec!["3".into(), "5".into()],
      default: Some("5".into()),
    }],
  });
  write_session(&path, &session);
  let mut session = load(&path);
  session.input = session.input.replace("> \n", "> /skip\n");
  let cfg = AppConfig {
    confirmations: Confirmations::Ask,
    ..yolo_cfg()
  };
  assert!(
    questions_resolvable(&session, &cfg),
    "/skip resolves in ask mode"
  );
  resolve_questions(&mut session, &cfg).expect("resolve");
  assert!(session.input.is_empty());
  assert!(
    session
      .request_messages()
      .last()
      .unwrap()
      .content
      .as_deref()
      .unwrap()
      .contains("\"answer\":\"5\""),
    "the default is used when skipping"
  );
}

#[test]
fn stream_writer_closes_sections_and_keeps_the_file_valid() {
  let dir = scratch("streamwriter");
  let path = dir.join("streamwriter.md");
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "hi".into();
  session.promote_input();
  session.begin_round();
  write_session(&path, &session);

  let mut w = StreamWriter::new(&path, load(&path));
  w.handle(&StreamEvent::Text("hello ".into()));
  w.handle(&StreamEvent::Text("world".into()));
  w.handle(&StreamEvent::Reasoning("a thought".into()));
  w.finish_section();

  let after = read_to_string(&path).unwrap();
  assert!(after.contains("hello world"), "{after}");
  assert!(after.contains("a thought"), "{after}");
  assert!(after.contains("<!-- slopfile:text -->"), "{after}");
  let parsed = load(&path);
  assert_eq!(parsed.section(), None, "sections closed: {after}");
  assert!(parsed.turn_in_progress(), "the turn is still open");
  assert!(parsed.turns[0].rounds[0].assistant.text.as_deref() == Some("hello world"));
}

#[test]
fn stream_translates_raw_think_tags_and_drops_orphans() {
  let dir = scratch("thinktags");
  let path = dir.join("thinktags.md");
  let mut session = template(&yolo_cfg(), &[]);
  session.begin_round();
  write_session(&path, &session);

  let mut w = StreamWriter::new(&path, load(&path));
  w.handle(&StreamEvent::Text("</think>answer text".into()));
  w.handle(&StreamEvent::Reasoning("hmm".into()));
  w.handle(&StreamEvent::Text("<think>ignored open".into()));
  w.finish_section();

  let after = read_to_string(&path).unwrap();
  assert!(
    !after.contains("<think>") && !after.contains("</think>"),
    "{after}"
  );
  assert!(after.contains("answer text"), "{after}");
  assert!(after.contains("ignored open"), "{after}");
  let parsed = load(&path);
  let round = &parsed.turns[0].rounds[0];
  assert_eq!(
    round.assistant.reasoning.as_deref(),
    Some("hmmignored open"),
    "a raw <think> keeps routing into reasoning"
  );
  assert_eq!(round.assistant.text.as_deref(), Some("answer text"));
}

#[test]
fn a_resumed_turn_restarts_the_assistant_region() {
  let dir = scratch("resume");
  let path = dir.join("resume.md");
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "go".into();
  session.promote_input();
  session.begin_round();
  session.push_reasoning("half a thought");
  session.push_text("half an answer");
  write_session(&path, &session);

  let parsed = load(&path);
  assert_eq!(parsed.section(), Some(session::TEXT));

  let mut resumed = parsed.clone();
  let (reasoning, text) = resumed.rewind_open_round();
  assert_eq!(reasoning.as_deref(), Some("half a thought"));
  assert_eq!(text.as_deref(), Some("half an answer"));
  resumed.push_reasoning(reasoning.as_deref().unwrap_or_default());
  resumed.push_text(text.as_deref().unwrap_or_default());
  resumed.push_text(" and the rest");
  write_session(&path, &resumed);
  let after = load(&path);
  assert_eq!(
    after.turns[0].rounds[0].assistant.text.as_deref(),
    Some("half an answer and the rest")
  );
  assert_eq!(
    after.turns[0].rounds[0].assistant.reasoning.as_deref(),
    Some("half a thought"),
    "the reasoning half is rewound and replayed too"
  );
}

#[test]
fn context_full_note_is_a_harness_note_and_not_work() {
  let dir = scratch("ctxfull");
  let path = dir.join("ctxfull.md");
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "hi".into();
  session.promote_input();
  session.begin_round();
  session.push_text("partial reply here");
  session.close_turn();
  session.add_note(session::NoteBy::Harness, flow::CONTEXT_FULL_NOTE);
  write_session(&path, &session);

  let parsed = load(&path);
  let notes = &parsed.turns[0].notes;
  assert_eq!(notes.len(), 1);
  assert_eq!(notes[0].by, session::NoteBy::Harness);
  assert!(
    next_action(&parsed, &yolo_cfg(), false) == Action::Idle,
    "a note alone is not work"
  );
  assert_eq!(
    next_action(&parsed, &yolo_cfg(), true),
    Action::Idle,
    "a full context needs the model to prune, not another turn"
  );
}

#[test]
fn typed_input_becomes_a_user_turn_only_when_armed() {
  let dir = scratch("draft");
  let path = dir.join("draft.md");
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "keep the daemons running".into();
  write_session(&path, &session);

  let cfg = AppConfig {
    confirmations: Confirmations::Ask,
    ..yolo_cfg()
  };
  let mut session = load(&path);
  session.promote_input();
  assert_eq!(
    session.turns[0].user.as_deref(),
    Some("keep the daemons running")
  );
  assert!(session.input.is_empty());
  assert_eq!(
    next_action(&session, &cfg, false),
    Action::Idle,
    "a plain save must not launch the model"
  );
  assert_eq!(
    next_action(&session, &cfg, true),
    Action::Generate,
    "armed means run"
  );
}

#[test]
fn session_template_has_default_header_and_prompt() {
  let cfg = yolo_cfg();
  let tpl = template(&cfg, &[]);
  let text = tpl.render();
  assert!(text.starts_with("```toml\n"), "{text}");
  assert!(text.contains("model = \"qwen3.8-27b\""), "{text}");
  assert!(text.contains(
    "tools = [\"read\", \"write\", \"edit\", \"bash\", \"grep\", \"find\", \"ls\", \"question\", \"subagent\"]"
  ));
  assert!(text.contains("<!-- slopfile:begin -->"), "{text}");
  assert!(text.contains("<!-- slopfile:system -->"), "{text}");
  assert!(text.ends_with("<!-- slopfile:input -->\n"), "{text}");
  assert!(!text.contains("# Session"), "{text}");
  assert!(text.contains("Current working directory: /tmp"), "{text}");

  let messages = tpl.request_messages();
  assert_eq!(messages.len(), 1);
  assert_eq!(messages[0].role, Role::System);
  let prompt = messages[0].content.as_deref().unwrap();
  assert!(prompt.contains("expert software engineering assistant"));
  assert!(prompt.contains("Context file: /t/1.md"), "{prompt}");
  assert!(
    !prompt.contains("# Session"),
    "heading stripped from prompt"
  );
}

#[test]
fn a_session_template_names_the_file_it_belongs_to() {
  let path = Path::new("/t/real/7.md");
  let tpl = session_template(path, &yolo_cfg(), &[]);
  assert!(
    tpl
      .system
      .contains(&format!("Context file: {}", path.display())),
    "{}",
    tpl.system
  );
}

#[test]
fn session_template_lists_skills() {
  let dir = scratch("skills");
  std::fs::write(
    dir.join("b-deploy.md"),
    "# Deploy checklist\n\nAlways run the tests before shipping.\n\nDetails here.\n",
  )
  .unwrap();
  std::fs::write(dir.join("a-style.md"), "# Style guide\nUse tabs.\n").unwrap();
  std::fs::write(dir.join("notes.txt"), "ignored, not markdown\n").unwrap();

  let mut cfg = yolo_cfg();
  cfg.skills_dir = dir.to_string_lossy().into_owned();
  let text = template(&cfg, &[]).render();
  assert!(
    text.contains("Skills available in"),
    "index missing: {text}"
  );
  assert!(text.contains("- a-style.md: Use tabs"), "{text}");
  assert!(text.contains("- b-deploy.md: Always run the tests before shipping."));
  assert!(
    !text.contains("notes.txt"),
    "non-md must be skipped: {text}"
  );

  std::fs::write(
    dir.join("c-front.md"),
    "---\nname: front-skill\ndescription: >\n  Handles frontmatter\n  skills properly.\nmetadata:\n  x: 1\n---\n\n# Body ignored\n",
  )
  .unwrap();
  let text2 = template(&cfg, &[]).render();
  assert!(
    text2.contains("- c-front.md: Handles frontmatter skills properly."),
    "frontmatter desc missing: {text2}"
  );

  std::fs::write(
    dir.join("d-manual.md"),
    "---\nname: manual-only\ndescription: Only for explicit requests.\ndisable-model-invocation: true\n---\n\n# Manual\n",
  )
  .unwrap();
  let text3 = template(&cfg, &[]).render();
  assert!(
    !text3.contains("d-manual.md"),
    "disabled skill skipped: {text3}"
  );
  assert!(text3.contains("- c-front.md:"), "other skills still listed");
}

#[test]
fn subagent_depth_and_spawn_filtering() {
  assert_eq!(subagent_depth("session"), 0);
  assert_eq!(subagent_depth("session.sub-01"), 1);
  assert_eq!(subagent_depth("session.sub-01.sub-02"), 2);

  let dir = scratch("spawn");
  let mut cfg = yolo_cfg();
  cfg.subagent_max_depth = 2;
  let parent = dir.join("session.md");
  write_session(&parent, &template(&cfg, &[]));

  let c1 = spawn_subagent(&parent, "do task one", &cfg).unwrap();
  assert_eq!(c1.file_name().unwrap(), "session.sub-01.md");
  let t1 = load(&c1);
  assert_eq!(t1.config.tools_excluded, vec!["question".to_string()]);
  assert_eq!(t1.input, "do task one\n", "the task is the pending input");
  assert!(
    t1.system
      .contains(&format!("Context file: {}", c1.display())),
    "a subagent must get the context paragraph for its own file"
  );

  let c2 = spawn_subagent(&c1, "nested task", &cfg).unwrap();
  assert_eq!(c2.file_name().unwrap(), "session.sub-01.sub-01.md");
  let t2 = load(&c2);
  assert_eq!(
    t2.config.tools_excluded,
    vec!["question".to_string(), "subagent".to_string()],
    "depth-2 child loses the subagent tool"
  );
  assert!(
    t2.system.contains("nobody will answer") && !t2.system.contains("call the `question` tool"),
    "prompt adapts to a missing question tool"
  );

  let err = spawn_subagent(&c2, "too deep", &cfg).unwrap_err();
  assert!(err.contains("depth limit"), "{err}");

  let c1b = spawn_subagent(&parent, "second child", &cfg).unwrap();
  assert_eq!(c1b.file_name().unwrap(), "session.sub-02.md");
}

#[tokio::test]
async fn tool_calls_enforce_exclusions_and_spawn_subagents() {
  let dir = scratch("blocks");
  let cfg = yolo_cfg();
  let parent = dir.join("blocks.md");
  let mut session = template(&cfg, &[]);
  session.config.tools_excluded = vec!["bash".to_string()];
  session.input = "hi".into();
  session.promote_input();
  write_session(&parent, &session);

  let after = tool_round(
    &parent,
    &cfg,
    &[ToolCall::new("1", "bash", r#"{"command":"echo hi"}"#)],
  )
  .await;
  assert!(
    last_results(&after)[0].output.contains("tool is disabled"),
    "{:?}",
    last_results(&after)
  );

  let after = tool_round(
    &parent,
    &cfg,
    &[ToolCall::new(
      "2",
      "subagent",
      r#"{"name":"w","task":"do it"}"#,
    )],
  )
  .await;
  assert!(last_results(&after)[0].output.contains("spawned at"));
  assert!(dir.join("blocks.sub-01.md").exists());
}

#[tokio::test]
async fn model_edits_of_its_own_context_file_are_synced() {
  let dir = scratch("ctxedit");
  let mut cfg = yolo_cfg();
  cfg.workdir = dir.to_string_lossy().into_owned();
  let session = dir.join("ctxedit.md");
  let mut start = template(&cfg, &[]);
  start.input = "hi".into();
  start.promote_input();
  write_session(&session, &start);

  let call = ToolCall::new(
    "1",
    "bash",
    json!({"command": format!(
      "sed -i 's/working on it/NOTES: only this matters/' {}",
      session.display()
    )})
    .to_string(),
  );
  let after = tool_round(&session, &cfg, &[call]).await;
  let text = after.render();
  assert_eq!(
    last_text(&after),
    Some("NOTES: only this matters"),
    "the model's edit is adopted: {text}"
  );
  assert!(after.pending_question().is_none());
  assert_eq!(last_results(&after).len(), 1);

  let wipe = ToolCall::new(
    "2",
    "bash",
    json!({"command": format!("printf 'everything is gone\\n' > {}", session.display())})
      .to_string(),
  );
  let rolled = tool_round(&session, &cfg, &[wipe]).await;
  let results = last_results(&rolled);
  assert!(
    results[0].output.contains("context edit rolled back"),
    "{:?}",
    results[0]
  );
  assert!(!results[0].ok, "a rolled back edit fails its tool");
  assert_eq!(rolled.system, start.system, "history survives the rollback");
}

#[test]
fn unfinished_subagents_block_the_parent() {
  let dir = scratch("wait-sub");
  let parent = dir.join("wait-sub.md");
  write_session(&parent, &template(&yolo_cfg(), &[]));
  let mut child = template(&yolo_cfg(), &[]);
  child.config.tools_excluded = vec!["question".to_string()];
  child.input = "task\n".into();
  write_session(&dir.join("wait-sub.sub-01.md"), &child);
  write_session(&dir.join("wait-sub.sub-02.md"), &child);
  std::fs::write(dir.join("wait-sub.sub-notes.md"), "unrelated\n").unwrap();

  let parent_session = load(&parent);
  let waiting = crate::daemon::unfinished_subagents(&parent, &parent_session).unwrap();
  assert_eq!(waiting.len(), 2, "{waiting:?}");

  let mut with_report = parent_session.clone();
  assert!(flow::inject_subagent_report(
    &mut with_report,
    "01",
    "all done",
    true
  ));
  assert!(has_report(&with_report, "01"));
  let waiting = crate::daemon::unfinished_subagents(&parent, &with_report).unwrap();
  assert_eq!(
    waiting.len(),
    1,
    "a reported child stops blocking: {waiting:?}"
  );

  assert!(
    !flow::inject_subagent_report(&mut with_report, "01", "all done", true),
    "a duplicate report is refused"
  );
}

#[tokio::test]
async fn deliver_subagent_result_injects_once() {
  let dir = scratch("deliver");
  let parent = dir.join("deliver.md");
  write_session(&parent, &template(&yolo_cfg(), &[]));
  let child = dir.join("deliver.sub-01.md");
  let mut sub = template(&yolo_cfg(), &[]);
  sub.config.tools_excluded = vec!["question".to_string()];
  sub.input = "task".into();
  sub.promote_input();
  sub.begin_round();
  sub.push_text("the full final report");
  sub.close_turn();
  write_session(&child, &sub);

  crate::daemon::deliver_subagent_result(&child, crate::daemon::SubagentReport::FromTranscript)
    .await
    .expect("deliver");

  let parent_text = read_to_string(&parent).unwrap();
  assert!(
    parent_text.contains("[system] Subagent '01' finished."),
    "{parent_text}"
  );
  assert!(
    parent_text.contains("the full final report"),
    "{parent_text}"
  );

  let loaded = load(&parent);
  let messages = loaded.request_messages();
  assert_eq!(messages.len(), 2, "system + report turn: {messages:?}");
  assert_eq!(messages[1].role, Role::User);
  assert!(
    messages[1]
      .content
      .as_deref()
      .unwrap()
      .contains("the full final report")
  );

  crate::daemon::deliver_subagent_result(&child, crate::daemon::SubagentReport::FromTranscript)
    .await
    .expect("deliver again");
  let once_more = read_to_string(&parent).unwrap();
  assert_eq!(
    once_more.matches("the full final report").count(),
    1,
    "no duplicate reports"
  );
  assert!(
    crate::daemon::unfinished_subagents(&parent, &load(&parent))
      .unwrap()
      .is_empty(),
    "the parent is unblocked"
  );
}

#[test]
fn creating_a_session_refuses_nonempty_and_reserved_names() {
  let dir = scratch("init");
  let cfg = yolo_cfg();

  let with_content = dir.join("busy.md");
  std::fs::write(&with_content, "user wrote this\n").unwrap();
  let err = control::create_session(&with_content, &cfg, &[]).unwrap_err();
  assert!(err.contains("not empty"), "{err}");
  assert_eq!(
    std::fs::read_to_string(&with_content).unwrap(),
    "user wrote this\n"
  );

  let reserved = dir.join("my.sub-notes.md");
  let err = control::create_session(&reserved, &cfg, &[]).unwrap_err();
  assert!(err.contains(".sub-"), "{err}");

  let empty = dir.join("fresh.md");
  std::fs::write(&empty, "").unwrap();
  control::create_session(&empty, &cfg, &[]).expect("empty file initializes");
  let created = load(&empty);
  assert!(
    created
      .system
      .contains("expert software engineering assistant")
  );
  assert!(created.input.is_empty(), "a fresh session waits for input");

  let err = control::create_session(&empty, &cfg, &[]).unwrap_err();
  assert!(
    err.contains("not empty"),
    "a second init must not clobber: {err}"
  );
}

#[tokio::test]
async fn control_round_trips_over_tcp() {
  let dir = scratch("control");
  let mut cfg = yolo_cfg();
  cfg.transcripts_dir = dir.to_string_lossy().into_owned();
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap().to_string();
  let served = cfg.clone();
  tokio::spawn(async move {
    let _ = control::serve_listener(listener, served).await;
  });

  write_session(&dir.join("ghost.md"), &template(&yolo_cfg(), &[]));

  let ask = |cmd: Cmd, session: &str| {
    let addr = addr.clone();
    let session = session.to_string();
    async move {
      control::send_command(
        &addr,
        &Req {
          cmd,
          session,
          answers: vec![],
        },
      )
      .await
      .expect("reply")
    }
  };

  let status = ask(Cmd::Status, "ghost").await;
  assert!(status.ok);
  assert_eq!(status.status.as_ref().unwrap().phase, control::Phase::Idle);

  let sent = ask(Cmd::Send, "ghost").await;
  assert!(sent.ok);
  assert!(sent.status.as_ref().unwrap().armed);

  let stopped = ask(Cmd::Stop, "ghost").await;
  assert!(!stopped.status.as_ref().unwrap().armed, "stop disarms");

  let aborted = ask(Cmd::Abort, "ghost").await;
  assert!(aborted.status.as_ref().unwrap().abort);
  let after = ask(Cmd::Status, "ghost").await;
  assert!(after.status.as_ref().unwrap().abort, "abort is sticky");

  let unknown = ask(Cmd::Status, "../etc/passwd").await;
  assert!(!unknown.ok, "path traversal is refused: {unknown:?}");
  assert_eq!(
    unknown.error.as_deref(),
    Some("no such task `../etc/passwd`")
  );

  let missing = ask(Cmd::Status, "no-such-task").await;
  assert!(!missing.ok, "a task with no file is an error: {missing:?}");
  assert_eq!(
    missing.error.as_deref(),
    Some("no such task `no-such-task`")
  );
}

#[tokio::test]
async fn answering_a_question_over_tcp_is_strict() {
  let dir = scratch("control-q");
  let mut cfg = yolo_cfg();
  cfg.transcripts_dir = dir.to_string_lossy().into_owned();
  write_session(&dir.join("pq.md"), &with_question(""));
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap().to_string();
  let served = cfg.clone();
  tokio::spawn(async move {
    let _ = control::serve_listener(listener, served).await;
  });

  let call = |answers: Vec<String>| {
    let addr = addr.clone();
    async move {
      control::send_command(
        &addr,
        &Req {
          cmd: Cmd::Question,
          session: "pq.md".to_string(),
          answers,
        },
      )
      .await
      .expect("reply")
    }
  };

  let wrong = call(vec!["1".into(), "2".into()]).await;
  assert!(!wrong.ok);
  assert_eq!(
    wrong.error.as_deref(),
    Some("failed to parse question answers, expected 1 answers, got 2"),
    "{wrong:?}"
  );

  let none = call(vec![]).await;
  assert!(!none.ok);
  assert_eq!(
    none.error.as_deref(),
    Some("failed to parse question answers, expected 1 answers, got 0")
  );

  let missing = control::send_command(
    &addr,
    &Req {
      cmd: Cmd::Question,
      session: "no-question.md".to_string(),
      answers: vec![],
    },
  )
  .await
  .expect("reply");
  assert!(!missing.ok);
  assert_eq!(
    missing.error.as_deref(),
    Some("no such task `no-question.md`"),
    "{missing:?}"
  );

  let ok = call(vec!["2".into()]).await;
  assert!(ok.ok, "{ok:?}");
  let after = load(&dir.join("pq.md"));
  assert!(after.pending_question().is_none());
  assert!(after.input.is_empty());
  assert!(
    after
      .request_messages()
      .last()
      .unwrap()
      .content
      .as_deref()
      .unwrap()
      .contains("\"answer\":\"blue\"")
  );
}

#[tokio::test]
async fn a_dead_daemon_reports_the_connect_error() {
  let err = control::send_command(
    "127.0.0.1:1",
    &Req {
      cmd: Cmd::Status,
      session: "x.md".to_string(),
      answers: vec![],
    },
  )
  .await
  .expect_err("nothing is listening on port 1");
  assert_eq!(
    err.to_string(),
    "unable to establish tcp connection, is the daemon running?"
  );
}

#[test]
fn arming_is_consumed_when_the_burst_settles() {
  let mut state = SessionState::default();
  let before = state.arm_token();
  assert!(!state.armed);
  state.settle(before);
  assert!(!state.armed, "an unarmed session stays unarmed");

  state.arm();
  let entry = state.arm_token();
  assert!(state.armed);
  state.settle(entry);
  assert!(!state.armed, "the burst that ran disarms at idle");

  state.arm();
  let entry = state.arm_token();
  state.arm();
  state.settle(entry);
  assert!(
    state.armed,
    "a send that landed mid-burst survives the settle"
  );

  let entry = state.arm_token();
  state.settle(entry);
  assert!(!state.armed, "and is consumed by the next idle pass");
}

#[test]
fn status_line_is_one_readable_row() {
  let mut state = SessionState::default();
  state.armed = true;
  state.phase = control::Phase::WaitingAnswer;
  state.pending_questions = 2;
  state.model = "qwen3.8-27b".into();
  let line = state.status("1.md").to_string();
  assert!(line.starts_with("task 1.md phase=waiting_answer"), "{line}");
  assert!(line.contains("armed=true"), "{line}");
  assert!(line.contains("pending_questions=2"), "{line}");
  assert!(!line.contains('\n'), "one line only: {line}");
}

#[tokio::test]
async fn tool_calls_next_to_a_question_are_still_executed_and_recorded() {
  let dir = scratch("qmix");
  let cfg = yolo_cfg();
  let path = dir.join("qmix.md");
  let mut start = template(&cfg, &[]);
  start.config.tools_excluded = vec!["subagent".to_string()];
  start.input = "check then ask".into();
  start.promote_input();
  write_session(&path, &start);

  let calls = [
    ToolCall::new("a1", "bash", r#"{"command":"echo ran"}"#),
    ToolCall::new(
      "a2",
      "question",
      r#"{"data":[{"question":"red?","answers":["red","blue"]}]}"#,
    ),
    ToolCall::new("a3", "bash", r#"{"command":"echo never"}"#),
  ];
  let mut session = load(&path);
  session.begin_round();
  session.push_text("checking something first");
  write_session(&path, &session);

  let at = calls.iter().position(|c| c.name == "question").unwrap();
  let mut outcomes = run_calls(&session, &calls[..at], &cfg, &path)
    .await
    .expect("before the question");
  let call_regions: Vec<session::Call> = calls
    .iter()
    .map(|c| session::Call {
      id: c.id.clone(),
      name: c.name.clone(),
      arguments: c.arguments.clone(),
    })
    .collect();
  let mut settled = settle_tool_round(session, call_regions, &mut outcomes, &path);
  flow::render_question(&mut settled, &calls[at]).expect("render the question");
  write_session(&path, &settled);

  let results = last_results(&settled);
  assert_eq!(results.len(), 3, "every call gets exactly one result");
  assert!(results[0].output.contains("ran"), "{:?}", results[0]);
  assert!(results[0].ok);
  assert_eq!(results[1].output, session::AWAITING_ANSWER);
  assert!(
    results[2].output.contains("not executed"),
    "a call after the question must say so: {:?}",
    results[2]
  );
  assert!(!results[2].ok);
  assert_eq!(settled.turns[0].rounds[0].assistant.calls.len(), 3);
  assert!(settled.pending_question().is_some());

  let messages = settled.request_messages();
  let ids: Vec<&str> = messages
    .iter()
    .filter_map(|m| m.tool_call_id.as_deref())
    .collect();
  assert_eq!(
    ids,
    vec!["c1", "c2", "c3"],
    "results follow the call order, one each, with short ids"
  );
  let call_ids: Vec<String> = messages
    .iter()
    .filter_map(|m| m.tool_calls.as_ref())
    .flatten()
    .map(|c| c.id.clone())
    .collect();
  assert_eq!(call_ids, vec!["c1", "c2", "c3"]);
  assert!(
    messages
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("ran"))),
    "the model still sees the work it asked for: {messages:?}"
  );
}

#[test]
fn only_the_tools_the_session_allows_are_advertised() {
  let mut cfg = template(&yolo_cfg(), &[]);
  assert!(crate::tools::advertised(&cfg.config).len() > 1);
  cfg.config.tools = Some(vec!["read".to_string(), "bash".to_string()]);
  let names: Vec<String> = crate::tools::advertised(&cfg.config)
    .into_iter()
    .map(|d| d.function.name)
    .collect();
  assert_eq!(names, vec!["read".to_string(), "bash".to_string()]);
  for name in &names {
    assert!(
      cfg.config.allows_tool(name),
      "{name} is advertised but refused"
    );
  }
  assert!(!names.iter().any(|n| n == "subagent" || n == "question"));

  cfg.config.tools = Some(vec!["read".to_string()]);
  cfg.config.tools_excluded = vec!["read".to_string()];
  assert!(
    crate::tools::advertised(&cfg.config).is_empty(),
    "an excluded tool is not advertised even when allowlisted"
  );
}

#[tokio::test]
async fn answering_over_tcp_arms_the_session_so_the_model_continues() {
  let dir = scratch("control-q2");
  let mut cfg = yolo_cfg();
  cfg.transcripts_dir = dir.to_string_lossy().into_owned();
  write_session(&dir.join("arm-me.md"), &with_question(""));
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap().to_string();
  let served = cfg.clone();
  tokio::spawn(async move {
    let _ = control::serve_listener(listener, served).await;
  });

  let resp = control::send_command(
    &addr,
    &Req {
      cmd: Cmd::Question,
      session: "arm-me.md".to_string(),
      answers: vec!["1".to_string()],
    },
  )
  .await
  .expect("reply");
  assert!(resp.ok, "{resp:?}");
  assert!(
    resp.status.as_ref().is_some_and(|s| s.armed),
    "answering must resume generation, not strand the session: {resp:?}"
  );

  let after = load(&dir.join("arm-me.md"));
  assert_eq!(
    next_action(
      &after,
      &AppConfig {
        confirmations: Confirmations::Ask,
        ..yolo_cfg()
      },
      true
    ),
    Action::Generate,
    "the answered question leaves work for the model"
  );
}

#[test]
fn content_after_the_fence_survives_every_render() {
  let preamble = "# my notes\n\n```toml\nmodel = \"m\"\n```\n\nkeep this line\n";
  let file = format!("{preamble}<!-- slopfile:begin -->\n<!-- slopfile:input -->\n");
  let (session, _) = session::parse(&file);
  let once = session.render();
  assert!(once.contains("\n\nkeep this line\n"), "{once}");
  let (twice, _) = session::parse(&once);
  assert_eq!(
    twice.render(),
    once,
    "rendering must not eat the blank line"
  );
  assert!(
    twice.preamble.ends_with("\n\nkeep this line"),
    "{:?}",
    twice.preamble
  );
}

#[test]
fn a_delivery_failure_leaves_the_child_reportable_again() {
  let dir = scratch("deliver-fail");
  let parent = dir.join("deliver-fail.md");
  write_session(&parent, &template(&yolo_cfg(), &[]));
  let child = dir.join("deliver-fail.sub-01.md");
  let mut sub = template(&yolo_cfg(), &[]);
  sub.input = "task".into();
  sub.promote_input();
  sub.begin_round();
  sub.push_text("the report");
  sub.close_turn();
  write_session(&child, &sub);

  // The parent cannot be parsed, so the report cannot be delivered and must not be marked done.
  std::fs::write(
    &parent,
    "```toml\nmodel = \"m\"\n```\n<!-- slopfile:begin -->\n<!-- /slopfile -->\n",
  )
  .unwrap();
  let err = tokio::runtime::Builder::new_current_thread()
    .build()
    .unwrap()
    .block_on(crate::daemon::deliver_subagent_result(
      &child,
      crate::daemon::SubagentReport::FromTranscript,
    ));
  assert!(err.is_err(), "{err:?}");
  assert!(
    !control::snapshot("deliver-fail.sub-01").delivered,
    "a failed delivery must stay retryable"
  );

  std::fs::write(&parent, template(&yolo_cfg(), &[]).render()).unwrap();
  tokio::runtime::Builder::new_current_thread()
    .build()
    .unwrap()
    .block_on(crate::daemon::deliver_subagent_result(
      &child,
      crate::daemon::SubagentReport::FromTranscript,
    ))
    .expect("retry delivers");
  assert!(control::snapshot("deliver-fail.sub-01").delivered);
  assert!(
    read_to_string(&parent).unwrap().contains("the report"),
    "the report lands on the retry"
  );
}

#[test]
fn every_session_name_normalizes_once() {
  assert_eq!(control::normalize("1").unwrap(), "1");
  assert_eq!(control::normalize("1.md").unwrap(), "1");
  assert_eq!(control::normalize(" 1.md ").unwrap(), "1");
  for bad in ["", "  ", "../x", "a/b", "a\\b", ".hidden", ".."] {
    assert!(control::normalize(bad).is_err(), "{bad:?} must be refused");
  }
  let cfg = yolo_cfg();
  assert_eq!(
    control::resolve_path(&cfg, "1.md").unwrap(),
    Path::new("/tmp").join("1.md")
  );
  assert_eq!(
    control::session_name(Path::new("/tmp/draft-007.md")),
    "draft-007"
  );
}

// ---- round-2 review regressions

#[test]
fn an_empty_response_does_not_keep_the_session_armed() {
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "hello".into();
  session.promote_input();
  session.begin_round();
  session.close_turn();
  assert!(
    !session.needs_generation(),
    "a round that produced nothing owes no reply: {:?}",
    session.request_messages()
  );
  assert_eq!(next_action(&session, &yolo_cfg(), true), Action::Idle);
}

#[test]
fn an_interrupted_round_still_gets_resumed() {
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "hello".into();
  session.promote_input();
  session.begin_round();
  assert!(
    session.needs_generation(),
    "a crash between begin_round and the first token must not strand the turn"
  );
  session.push_text("half");
  assert!(session.needs_generation());
  session.close_turn();
  assert!(!session.needs_generation(), "a finished reply owes nothing");
}

#[test]
fn tool_results_alone_still_demand_a_reply() {
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "go".into();
  session.promote_input();
  session.begin_round();
  session.finish_round(
    vec![session::Call {
      id: "c1".into(),
      name: "bash".into(),
      arguments: "{}".into(),
    }],
    vec![session::ToolOutcome {
      id: "c1".into(),
      name: "bash".into(),
      ok: true,
      ms: 0,
      output: "out".into(),
    }],
  );
  assert!(
    session.needs_generation(),
    "an unanswered tool result means the model has to speak"
  );
}

#[tokio::test]
async fn a_pruned_report_turn_does_not_wedge_the_parent() {
  let dir = scratch("wedge");
  let parent = dir.join("wedge.md");
  write_session(&parent, &template(&yolo_cfg(), &[]));
  let mut child = template(&yolo_cfg(), &[]);
  child.input = "task\n".into();
  child.promote_input();
  child.begin_round();
  child.push_text("the report");
  child.close_turn();
  let child_path = dir.join("wedge.sub-01.md");
  write_session(&child_path, &child);

  crate::daemon::deliver_subagent_result(
    &child_path,
    crate::daemon::SubagentReport::FromTranscript,
  )
  .await
  .expect("deliver");
  assert!(read_to_string(&parent).unwrap().contains("the report"));

  // The model prunes the turn that holds the report, which its prompt explicitly invites.
  write_session(&parent, &template(&yolo_cfg(), &[]));
  assert!(
    crate::daemon::unfinished_subagents(&parent, &load(&parent))
      .unwrap()
      .is_empty(),
    "a delivered child stays delivered even if its report is pruned"
  );
}

#[tokio::test]
async fn concurrent_report_deliveries_do_not_overwrite_each_other() {
  let dir = scratch("deliver-race");
  let parent = dir.join("race.md");
  write_session(&parent, &template(&yolo_cfg(), &[]));
  let mut children = Vec::new();
  for n in ["01", "02", "03", "04"] {
    let path = dir.join(format!("race.sub-{n}.md"));
    let mut child = template(&yolo_cfg(), &[]);
    child.input = "task\n".into();
    child.promote_input();
    child.begin_round();
    child.push_text(&format!("report-{n}"));
    child.close_turn();
    write_session(&path, &child);
    children.push(path);
  }
  let mut tasks = Vec::new();
  for path in children.iter() {
    let path = path.clone();
    tasks.push(tokio::spawn(async move {
      crate::daemon::deliver_subagent_result(&path, crate::daemon::SubagentReport::FromTranscript)
        .await
    }));
  }
  for task in tasks {
    task.await.expect("join").expect("deliver");
  }
  let text = read_to_string(&parent).unwrap();
  for n in ["01", "02", "03", "04"] {
    assert!(
      text.contains(&format!("report-{n}")),
      "report-{n} was lost:\n{text}"
    );
  }
}

#[tokio::test]
async fn concurrent_writes_leave_the_file_intact() {
  let dir = scratch("atomic");
  let path = dir.join("atomic.md");
  write_session(&path, &template(&yolo_cfg(), &[]));
  let filler = "x".repeat(200_000);
  let probe = filler[..1000].to_string();
  let mut readers = Vec::new();
  for _ in 0..4 {
    let path = path.clone();
    let probe = probe.clone();
    readers.push(tokio::task::spawn_blocking(move || {
      for _ in 0..50 {
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
          text.contains("slopfile:system") || text.contains(&probe),
          "a reader caught a torn file: {} bytes",
          text.len()
        );
      }
    }));
  }
  let mut writers = Vec::new();
  for n in 0..8 {
    let path = path.clone();
    let filler = filler.clone();
    writers.push(tokio::task::spawn_blocking(move || {
      let mut session = crate::transcript::load(&path).expect("load");
      session.add_note(session::NoteBy::Model, format!("note {n} {filler}"));
      crate::transcript::save(&path, &session).expect("save");
    }));
  }
  for writer in writers {
    writer.await.expect("join");
  }
  for reader in readers {
    reader.await.expect("join");
  }
  let leftovers: Vec<_> = std::fs::read_dir(&dir)
    .unwrap()
    .flatten()
    .map(|e| e.file_name().to_string_lossy().into_owned())
    .filter(|n| n.ends_with(".tmp"))
    .collect();
  assert!(
    leftovers.is_empty(),
    "temp files left behind: {leftovers:?}"
  );
  // Concurrent read-modify-write keeps only the last writer's view; what must hold is that the
  // file on disk is always a complete, parseable session and no scratch file is left behind.
  let final_text = read_to_string(&path).unwrap();
  assert!(final_text.contains("slopfile:system"), "{final_text}");
  let (parsed, report) = crate::session::parse(&final_text);
  assert!(report.is_clean(), "{:?}", report.problems());
  assert!(
    parsed.render() == final_text,
    "the survivor is a fixed point"
  );
}

// ---- round-3 review regressions

#[test]
fn the_prompt_budget_leaves_room_for_the_answer() {
  let budget = 131_072;
  let allowance = tokens::output_allowance(budget);
  assert_eq!(allowance, 40_000);
  assert_eq!(
    tokens::prompt_budget(budget),
    budget - allowance,
    "prompt + answer must fit the window"
  );
  assert_eq!(tokens::prompt_budget(0), 0);
  assert_eq!(
    tokens::prompt_budget(3),
    0,
    "no underflow when the allowance exceeds the window"
  );
}

#[test]
fn a_truncated_window_never_opens_with_the_assistant() {
  let mut msgs = vec![ChatMessage::system("sys")];
  for i in 1..=8 {
    msgs.push(ChatMessage::user(format!("user {i}")));
    msgs.push(ChatMessage::assistant(format!("assistant {i}")));
  }
  let counts: Vec<usize> = msgs.iter().map(|_| 3).collect();
  for budget in [6usize, 8, 10, 12, 14, 16, 18, 20, 24] {
    let (window, kept) = tokens::truncate_messages(msgs.clone(), &counts, budget as u32);
    let roles: Vec<Role> = window.iter().map(|m| m.role).collect();
    assert_eq!(roles[0], Role::System, "budget {budget}");
    if kept <= budget {
      assert_ne!(
        roles.get(1),
        Some(&Role::Assistant),
        "budget {budget} opened with an assistant message: {roles:?}"
      );
    } else {
      // Over budget: the caller must refuse rather than silently send an invalid window.
      assert_ne!(
        roles.get(1),
        Some(&Role::Assistant),
        "budget {budget}: {roles:?}"
      );
    }
  }
}

#[tokio::test]
async fn a_relative_extra_read_root_resolves_for_read() {
  let root = scratch("read-roots");
  let workdir = root.join("workspace");
  let skills = root.join("shared/skills");
  std::fs::create_dir_all(&workdir).unwrap();
  std::fs::create_dir_all(&skills).unwrap();
  std::fs::write(skills.join("guide.md"), "# Guide\nbody\n").unwrap();
  // The extra root is what the system prompt points at, searched with the same relative path.

  let relative = tools::run(
    "read",
    r#"{"path":"shared/skills/guide.md"}"#,
    workdir.clone(),
    30_000,
    std::slice::from_ref(&root),
    None,
  )
  .await;
  assert!(
    relative.ok,
    "a valid path that is simply absent from the workdir must fall back to the extra roots: {}",
    relative.output
  );

  let absolute = tools::run(
    "read",
    &format!(
      r#"{{"path":{:?}}}"#,
      skills.join("guide.md").to_string_lossy()
    ),
    workdir.clone(),
    30_000,
    std::slice::from_ref(&root),
    None,
  )
  .await;
  assert!(absolute.ok, "{}", absolute.output);

  let escaped = tools::run(
    "read",
    r#"{"path":"../shared/skills/guide.md"}"#,
    workdir.clone(),
    30_000,
    &[PathBuf::from("shared/skills")],
    None,
  )
  .await;
  assert!(
    !escaped.ok,
    "an escape must still be refused: {}",
    escaped.output
  );
}

// ---- round-4 review regressions

#[test]
fn a_hand_edited_fence_survives_a_save_even_behind_prose() {
  // The fence fails to parse as TOML, so we must keep it verbatim. Dropping it would reset every
  // per-session setting *and* leave the file in a shape the model's own later edits are refused for.
  let content = "\
# campaign notes

```toml
model = special-model
```

keep me
<!-- slopfile:begin -->
<!-- slopfile:system -->
sys
<!-- /slopfile -->
<!-- slopfile:input -->
";
  let p = scratch("fence-prose").join("s.md");
  std::fs::write(&p, content).unwrap();
  let s = transcript::load(&p).expect("clean parse");
  let out = s.render();
  assert!(
    out.contains("model = special-model"),
    "the fence was deleted:\n{out}"
  );
  assert!(out.contains("# campaign notes"), "prose was lost:\n{out}");
  assert!(out.contains("keep me"), "prose was lost:\n{out}");
  assert_eq!(
    out.matches("```toml").count(),
    1,
    "the fence was duplicated:\n{out}"
  );

  // And the file the model is shown must still be editable by the model.
  assert!(
    crate::session::adopt(&out).map(drop).is_ok(),
    "the file we just wrote is one we now refuse to edit: {:?}",
    crate::session::adopt(&out).map(drop)
  );
}

#[test]
fn text_the_user_types_during_a_stream_is_not_clobbered() {
  use crate::stream::StreamWriter;
  let p = scratch("stream-clobber").join("s.md");
  std::fs::write(
    &p,
    "<!-- slopfile:begin -->\n<!-- slopfile:system -->\nsys\n<!-- /slopfile -->\n<!-- slopfile:input -->\n",
  )
  .unwrap();
  let s = transcript::load(&p).expect("parse");
  let mut w = StreamWriter::new(&p, s);

  w.handle(&crate::types::StreamEvent::Text("hello".into()));
  w.finish_section();

  // The human types and saves while the answer is still streaming.
  let typed = "<!-- slopfile:begin -->\n<!-- slopfile:system -->\nsys\n<!-- /slopfile -->\n<!-- slopfile:turn -->\n<!-- slopfile:user -->\nq\n<!-- /slopfile -->\n<!-- slopfile:assistant -->\n<!-- slopfile:text -->\nhello\n<!-- /slopfile -->\n<!-- /slopfile -->\n<!-- slopfile:input -->\nPLEASE ALSO REWRITE THE PARSER\n";
  std::fs::write(&p, typed).unwrap();

  w.handle(&crate::types::StreamEvent::Text(" world".into()));
  w.finish_section();

  let after = std::fs::read_to_string(&p).unwrap();
  assert!(
    after.contains("PLEASE ALSO REWRITE THE PARSER"),
    "the user's message was deleted by a flush:\n{after}"
  );
  assert!(
    after.contains("hello"),
    "streamed output was lost:\n{after}"
  );
}

#[tokio::test]
async fn a_dangling_symlink_cannot_carry_a_write_out_of_the_workdir() {
  let root = scratch("dangle-escape");
  let wd = root.join("wd");
  let outside = root.join("outside");
  std::fs::create_dir_all(&wd).unwrap();
  std::fs::create_dir_all(&outside).unwrap();
  // The classic real-world case: a link whose target does not exist *yet*.
  std::os::unix::fs::symlink(outside.join("planted.txt"), wd.join("dangle")).unwrap();

  let r = tools::run(
    "write",
    r#"{"path":"dangle","content":"ESCAPED"}"#,
    wd.clone(),
    30_000,
    &[],
    None,
  )
  .await;
  assert!(
    !r.ok,
    "the write was allowed through a dangling symlink: {}",
    r.output
  );
  assert!(
    !outside.join("planted.txt").exists(),
    "a file was planted outside the workdir"
  );
}

#[tokio::test]
async fn truncating_a_command_is_not_reported_as_a_failed_command() {
  let wd = scratch("big-output");
  std::fs::create_dir_all(&wd).unwrap();
  let r = tools::run(
    "bash",
    r#"{"command":"seq 1 20000"}"#,
    wd.clone(),
    30_000,
    &[],
    None,
  )
  .await;
  // We drain to EOF rather than closing the pipe at the cap, so the command runs to completion
  // and its exit status is genuine: this one really did succeed.
  assert!(
    r.ok,
    "a successful command was reported as a failure: {}",
    r.output
  );
  assert!(
    r.output.contains("truncated"),
    "the model is not told the output was cut: {:?}",
    r.output.chars().rev().take(160).collect::<String>()
  );
}

#[tokio::test]
async fn read_output_is_capped() {
  let wd = scratch("read-cap");
  std::fs::create_dir_all(&wd).unwrap();
  std::fs::write(wd.join("big.txt"), "x".repeat(5_000_000)).unwrap();
  let r = tools::run(
    "read",
    r#"{"path":"big.txt","limit":100000000}"#,
    wd.clone(),
    30_000,
    &[],
    None,
  )
  .await;
  assert!(
    r.output.len() < 100_000,
    "read returned {} bytes uncapped",
    r.output.len()
  );
  assert!(
    r.output.contains("truncated"),
    "no note that it was cut: {:?}",
    &r.output[..r.output.len().min(200)]
  );
}

#[test]
fn a_control_request_is_bounded() {
  // No auth on the control port, so neither the line nor the session name may be unbounded.
  assert!(
    crate::control::normalize(&"a".repeat(10_000)).is_err(),
    "an 10k session name was accepted"
  );
  assert!(crate::control::normalize("reasonable-name").is_ok());
}

#[test]
fn a_session_read_back_with_an_unanswered_call_still_builds_a_valid_request() {
  // Exactly what a model pruning its own transcript produces: two announced calls, one result.
  let raw = "\
<!-- slopfile:begin -->
<!-- slopfile:system -->
sys
<!-- /slopfile -->
<!-- slopfile:turn -->
<!-- slopfile:user -->
q
<!-- /slopfile -->
<!-- slopfile:assistant -->
<!-- slopfile:call {\"id\":\"c1\",\"name\":\"bash\",\"arguments\":\"{}\"} -->
<!-- /slopfile -->
<!-- slopfile:call {\"id\":\"c2\",\"name\":\"read\",\"arguments\":\"{}\"} -->
<!-- /slopfile -->
<!-- /slopfile -->
<!-- slopfile:result {\"id\":\"c1\",\"name\":\"bash\",\"ok\":true,\"ms\":1} -->
out
<!-- /slopfile -->
<!-- /slopfile -->
<!-- slopfile:input -->
";
  let p = scratch("unpaired").join("s.md");
  std::fs::write(&p, raw).unwrap();
  let s = transcript::load(&p).expect("a pruned call is still a clean parse");
  let msgs = s.request_messages();
  let announced: usize = msgs
    .iter()
    .filter_map(|m| m.tool_calls.as_ref())
    .map(|c| c.len())
    .sum();
  let answered = msgs.iter().filter(|m| m.role == Role::Tool).count();
  assert_eq!(
    announced, answered,
    "every announced call needs a result or the server rejects the request: {msgs:?}"
  );
  let ids: Vec<&str> = msgs
    .iter()
    .filter_map(|m| m.tool_call_id.as_deref())
    .collect();
  let mut uniq = ids.clone();
  uniq.sort_unstable();
  uniq.dedup();
  assert_eq!(ids.len(), uniq.len(), "duplicate tool_call_id: {ids:?}");
}

#[test]
fn a_question_cannot_answer_itself() {
  let qs = vec![crate::questions::Question {
    id: 1,
    question: "Ship it?\n> 1".into(),
    answers: vec!["yes".into(), "no".into()],
    default: None,
  }];
  let rendered = crate::questions::render_block(&qs);
  assert_eq!(
    crate::questions::answer_text(&rendered),
    None,
    "the model answered its own question: {:?}",
    crate::questions::answer_text(&rendered)
  );
}

#[tokio::test]
async fn a_failed_command_is_not_laundered_into_a_success_by_truncation() {
  // Both too much output AND a real failure: the exit code is the command's own, so it failed.
  let wd = scratch("failed-truncated");
  std::fs::create_dir_all(&wd).unwrap();
  let r = tools::run(
    "bash",
    r#"{"command":"i=0; while [ $i -lt 20000 ]; do echo \"line $i aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"; i=$((i+1)); done; exit 3"}"#,
    wd.clone(),
    30_000,
    &[],
    None,
  )
  .await;
  // Truncation must not launder a real failure: the command exits 3, so it failed.
  assert!(!r.ok, "a command that exited 3 was reported as success");
  assert!(
    r.output.contains("exit 3"),
    "the real exit code is missing: {:?}",
    r.output.chars().rev().take(160).collect::<String>()
  );
}

#[tokio::test]
async fn a_killed_command_that_was_not_truncated_is_still_a_failure() {
  let wd = scratch("killed");
  std::fs::create_dir_all(&wd).unwrap();
  let r = tools::run(
    "bash",
    r#"{"command":"kill -9 $$"}"#,
    wd.clone(),
    30_000,
    &[],
    None,
  )
  .await;
  assert!(
    !r.ok,
    "a command killed by a signal was called a success: {}",
    r.output
  );
}

#[test]
fn the_model_cannot_skip_its_own_question() {
  let qs = vec![crate::questions::Question {
    id: 1,
    question: "Ship it?\n/skip".into(),
    answers: vec!["yes".into(), "no".into()],
    default: Some("yes".into()),
  }];
  let rendered = crate::questions::render_block(&qs);
  assert!(
    !crate::questions::is_skip(&rendered),
    "a /skip line in the model's own question text auto-resolved the question"
  );
}

#[test]
fn a_container_style_path_in_the_shared_config_resolves_on_the_host() {
  // shared/slopfile.toml is read by both sides: the daemon wants "/shared/transcripts", but `sf`
  // runs on the host where that path is not writable. They are the same bind-mounted directory,
  // so the leading slash must not decide where the host looks.
  let root = std::path::Path::new("/repo");
  assert_eq!(
    crate::repo_relative("/shared/transcripts", root),
    "/repo/shared/transcripts"
  );
  assert_eq!(
    crate::repo_relative("shared/transcripts", root),
    "/repo/shared/transcripts"
  );
  // Anything that is not a repo-relative `shared/` path is left alone.
  assert_eq!(crate::repo_relative("/workspace", root), "/workspace");
  assert_eq!(crate::repo_relative("/tmp/mine", root), "/tmp/mine");
  assert_eq!(crate::repo_relative("workspace", root), "workspace");
}

#[test]
fn a_freshly_created_transcript_explains_itself_and_survives_a_round_trip() {
  let p = scratch("self-describing").join("s.md");
  let cfg = crate::load_app_config();
  crate::control::create_session(&p, &cfg, &[]).expect("create");

  let text = std::fs::read_to_string(&p).unwrap();
  // The managed block is rendered without a sentinel fence; it opens with the path line.
  assert!(text.contains("This conversation is that file"));
  assert!(text.contains(&format!("Context file: {}", p.display())));

  // The managed block contains literal `<`, so it is escaped on render. It must come back
  // byte-identical, or the model would read a corrupted copy of its own instructions.
  let s = transcript::load(&p).expect("parse");
  assert_eq!(
    s.render(),
    text,
    "the managed block did not survive parse->render"
  );
  assert!(
    s.system.contains("<!-- slopfile:NAME -->"),
    "markers came back escaped"
  );
}

// ---- short tool-call ids

#[test]
fn ids_are_shortened_per_round_and_still_pair() {
  let mut s = Session::default();
  s.turns.push(session::Turn::default());
  s.begin_round();
  s.push_text("calling");
  let long = "k4gU87g6xuj5cokczZ2Fq6BqxGdfosGX";
  s.finish_round(
    vec![
      crate::session::Call {
        id: long.into(),
        name: "bash".into(),
        arguments: "{}".into(),
      },
      crate::session::Call {
        id: "BvPQlIyfPvWQm7P0l7RqaSxrAMfbjHte".into(),
        name: "read".into(),
        arguments: "{}".into(),
      },
    ],
    vec![crate::session::ToolOutcome {
      id: long.into(),
      name: "bash".into(),
      ok: true,
      ms: 1,
      output: "a".into(),
    }],
  );
  let round = &s.turns[0].rounds[0];
  assert_eq!(round.assistant.calls[0].id, "c1");
  assert_eq!(
    round.results[0].id, "c1",
    "a result must keep its call's id"
  );
  assert_eq!(
    round.assistant.calls[1].id, "c2",
    "an unanswered call still gets its own id"
  );
  let ids: Vec<&str> = round
    .assistant
    .calls
    .iter()
    .map(|c| c.id.as_str())
    .collect();
  assert_eq!(ids, vec!["c1", "c2"], "ids are short and sequential");
}

#[test]
fn shortening_is_stable_across_repeated_passes() {
  let mut s = Session::default();
  s.turns.push(session::Turn::default());
  s.begin_round();
  s.finish_round(
    vec![crate::session::Call {
      id: "call_abc123".into(),
      name: "bash".into(),
      arguments: "{}".into(),
    }],
    vec![crate::session::ToolOutcome {
      id: "call_abc123".into(),
      name: "bash".into(),
      ok: true,
      ms: 1,
      output: "x".into(),
    }],
  );
  let once = s.render();
  s.shorten_ids();
  s.shorten_ids();
  assert_eq!(
    s.render(),
    once,
    "shortening twice must not renumber c1 -> c2"
  );
}

#[test]
fn a_subagent_is_never_offered_the_question_tool() {
  use crate::flow;
  let cfg = AppConfig::default();
  // `question` is excluded when a subagent transcript is created, so the model can only reach the
  // name by inventing it; the daemon refuses that rather than answering on the user's behalf.
  let path = scratch("subagent-question").join("parent.sub-01.md");
  let child = flow::subagent_task_template(&path, &cfg, &["question".to_string()], "do the thing");
  assert!(
    !crate::tools::advertised(&child.config)
      .iter()
      .any(|t| t.function.name == "question"),
    "question must not be offered to a subagent"
  );
  std::fs::write(&path, child.render()).unwrap();
  assert!(
    flow::is_subagent_path(&path),
    "{} must look like a subagent",
    path.display()
  );
  // And a parent session keeps it.
  let parent = template(&cfg, &[]);
  assert!(
    crate::tools::advertised(&parent.config)
      .iter()
      .any(|t| t.function.name == "question")
  );
}

// ---- backgrounding a job must not hang the call

#[tokio::test]
async fn backgrounding_a_long_job_returns_immediately_with_its_output() {
  let wd = scratch("bg-job");
  std::fs::create_dir_all(&wd).unwrap();
  // `A && B & C` backgrounds a *subshell* whose stdout is still ours, so waiting for pipe EOF
  // hung this call until the timeout and then the group kill destroyed the job.
  let r = tools::run(
    "bash",
    &json!({
      "command": format!("cd {} && nohup sleep 120 > bg.log 2>&1 & echo \"started pid $!\"", wd.display())
    })
    .to_string(),
    wd.clone(),
    20_000,
    &[],
    None,
  )
  .await;
  assert!(r.ok, "backgrounding a job should succeed: {}", r.output);
  assert!(
    r.output.contains("started pid"),
    "the foreground output was thrown away: {:?}",
    r.output
  );
  let pid: i32 = r
    .output
    .split_whitespace()
    .nth(2)
    .and_then(|p| p.parse().ok())
    .expect("a pid in the output");
  let alive = std::process::Command::new("kill")
    .args(["-0", &pid.to_string()])
    .status()
    .map(|s| s.success())
    .unwrap_or(false);
  assert!(
    alive,
    "the harness killed the job it had just launched (pid {pid})"
  );
  let _ = std::process::Command::new("kill")
    .args(["-9", &pid.to_string()])
    .status();
}

#[tokio::test]
async fn a_hung_foreground_command_still_times_out_and_kills_its_children() {
  let wd = scratch("bg-hang");
  std::fs::create_dir_all(&wd).unwrap();
  let pid_file = wd.join("bg.pid");
  let r = tools::run(
    "bash",
    &json!({ "command": format!("sleep 120 & echo $! > {}; sleep 120", pid_file.display()) })
      .to_string(),
    wd.clone(),
    400,
    &[],
    None,
  )
  .await;
  assert!(!r.ok, "a hung command must time out");
  assert!(r.output.contains("timeout"), "{}", r.output);
  let pid: i32 = std::fs::read_to_string(&pid_file)
    .unwrap()
    .trim()
    .parse()
    .unwrap();
  tokio::time::sleep(std::time::Duration::from_millis(200)).await;
  let alive = std::process::Command::new("kill")
    .args(["-0", &pid.to_string()])
    .status()
    .map(|s| s.success())
    .unwrap_or(false);
  assert!(
    !alive,
    "the process-group kill missed the background child {pid}"
  );
}

#[tokio::test]
async fn a_command_producing_too_much_output_still_runs_to_completion() {
  let wd = scratch("cap-complete");
  std::fs::create_dir_all(&wd).unwrap();
  let started = std::time::Instant::now();
  let r = tools::run(
    "bash",
    r#"{"command":"seq 1 20000; echo TAILMARKER"}"#,
    wd.clone(),
    20_000,
    &[],
    None,
  )
  .await;
  assert!(
    started.elapsed() < std::time::Duration::from_secs(10),
    "stopping the read at the cap blocks the child on a full pipe: {:?}",
    started.elapsed()
  );
  assert!(
    r.ok,
    "a successful command was called a failure: {}",
    r.output
  );
  assert!(
    !r.output.contains("TAILMARKER"),
    "the tail should have been cut, proving truncation happened"
  );
  assert!(
    r.output.contains("truncated"),
    "no truncation note: {:?}",
    &r.output[..80]
  );
}

/// The reason a burst stopped has to be readable in the transcript. Leaving it in the daemon log
/// made "the provider refused" and "the model finished" look identical from the outside -- both
/// leave the file ending in an empty input region, silently.
#[test]
fn a_provider_refusal_is_recorded_in_the_transcript_itself() {
  use crate::api::ApiError;
  let dir = scratch("provider-failure");
  let path = dir.join("1.md");
  write_session(&path, &template(&yolo_cfg(), &[]));

  let e = ApiError::NonOkStatus(
    429,
    r#"{"error":{"message":"Rate limit exceeded: free-models-per-day"}}"#.to_string(),
    false,
  );
  daemon::record_provider_failure(&path, load(&path), &e).unwrap();
  let text = read_to_string(&path).unwrap();
  assert!(
    text.contains("Rate limit exceeded: free-models-per-day"),
    "the provider's own sentence, not a status code: {text}"
  );
  // Written as a comment region, so it never reads as the model having said it.
  assert!(text.contains("<!-- slopfile:note"), "{text}");

  // Sending again against the same dead endpoint must not bury the file it is explaining.
  for _ in 0..3 {
    daemon::record_provider_failure(&path, load(&path), &e).unwrap();
  }
  let text = read_to_string(&path).unwrap();
  assert_eq!(
    text.matches("this turn was never answered").count(),
    1,
    "one refusal must not become four notes: {text}"
  );
}

/// A 4xx is returned before any token arrives, so nothing has flushed since the snapshot was
/// taken. Saving that snapshot wholesale deletes whatever the user typed while the request was in
/// flight -- and a refusal is exactly when someone retypes their message.
#[test]
fn a_refusal_does_not_delete_input_typed_while_the_request_was_in_flight() {
  use crate::api::ApiError;
  let dir = scratch("failure-input");
  let path = dir.join("1.md");
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "original request".into();
  session.promote_input();
  session.begin_round();
  write_session(&path, &session);

  // The snapshot the StreamWriter holds, taken before the request went out.
  let snapshot = load(&path);
  // Meanwhile the user saves a correction.
  let mut on_disk = load(&path);
  on_disk.input = "ALSO DO THIS".into();
  write_session(&path, &on_disk);

  daemon::record_provider_failure(
    &path,
    snapshot,
    &ApiError::NonOkStatus(429, r#"{"error":{"message":"nope"}}"#.to_string(), false),
  )
  .unwrap();

  let after = load(&path);
  assert_eq!(
    after.input.trim(),
    "ALSO DO THIS",
    "the human's newer words must survive the note"
  );
}

/// The overflow note tells the user to shrink this transcript. If they do that while the failed
/// request was still in flight, the edit must stick -- otherwise the advice is self-defeating.
#[test]
fn a_failure_note_does_not_revert_edits_made_while_the_request_was_running() {
  use crate::api::ApiError;
  let dir = scratch("failure-edit");
  let path = dir.join("1.md");

  // Two turns on disk; the request is made against both.
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "first".into();
  session.promote_input();
  session.begin_round();
  session.push_text("an old answer nobody needs");
  session.finish_round(vec![], vec![]);
  session.input = "second".into();
  session.promote_input();
  session.begin_round();
  write_session(&path, &session);
  let snapshot = load(&path);

  // Mid-request, the user prunes the first turn.
  let mut edited = load(&path);
  edited.turns.remove(0);
  write_session(&path, &edited);

  daemon::record_provider_failure(
    &path,
    snapshot,
    &ApiError::NonOkStatus(
      400,
      r#"{"error":{"message":"This model's maximum context length is 512000 tokens"}}"#.to_string(),
      false,
    ),
  )
  .unwrap();

  let after = load(&path);
  assert_eq!(
    after.turns.len(),
    1,
    "the user's prune must not be undone by the failure note"
  );
  assert!(
    after
      .turns
      .last()
      .unwrap()
      .notes
      .iter()
      .any(|n| n.text.contains("too large")),
    "and the note must still be there"
  );
}

/// If the user prunes the dangling assistant region mid-generation, the last round on disk becomes
/// an already-answered one. Grafting the aborted fragment onto it by position would rewrite a
/// real answer as a half-sentence, and the transcript would assert the model said something it
/// never said -- while still carrying that round's tool calls and results.
#[test]
fn an_aborted_fragment_is_not_grafted_onto_a_finished_round() {
  use crate::api::ApiError;
  use crate::session::Call;
  let dir = scratch("failure-graft");
  let path = dir.join("1.md");

  let mut session = template(&yolo_cfg(), &[]);
  session.input = "do two things".into();
  session.promote_input();
  // Round 1 completes properly, with a tool call and its result.
  session.begin_round();
  session.push_text("the real answer to round one");
  session.finish_round(
    vec![Call {
      id: "c1".into(),
      name: "read".into(),
      arguments: "{}".into(),
    }],
    vec![crate::session::ToolOutcome {
      id: "c1".into(),
      name: "read".into(),
      output: "file contents".into(),
      ok: true,
      ms: 1,
    }],
  );
  // Round 2 is in flight when the request dies.
  session.begin_round();
  session.push_text("I was partway through");
  write_session(&path, &session);
  let snapshot = load(&path);

  // Mid-flight the user deletes the dangling open region, leaving round 1 as the last round.
  let mut edited = load(&path);
  edited.turns[0].rounds.truncate(1);
  edited.close_turn();
  write_session(&path, &edited);

  let pre = load(&path);
  {
    let r = pre.turns.last().unwrap().rounds.last().unwrap();
    println!(
      "PRE-GRAFT rounds={} calls={} results={} text={:?}",
      pre.turns.last().unwrap().rounds.len(),
      r.assistant.calls.len(),
      r.results.len(),
      r.assistant.text
    );
  }
  daemon::record_provider_failure(
    &path,
    snapshot,
    &ApiError::Timeout(std::time::Duration::from_secs(300)),
  )
  .unwrap();

  let after = load(&path);
  let last = after.turns.last().unwrap();
  let round = last.rounds.last().unwrap();
  assert_eq!(
    round.assistant.text.as_deref(),
    Some("the real answer to round one"),
    "a completed round's answer must never be overwritten by an aborted fragment"
  );
  assert!(
    !round.assistant.calls.is_empty(),
    "and its tool call must stay paired with it: {round:?}"
  );
}

/// The ordinary end of a turn leaves a round with no calls and no results, so that alone cannot
/// tell a finished answer from one still being written. Here the previous round is prose and the
/// user prunes the dangling region, which is the case that guard alone would get wrong.
#[test]
fn an_aborted_fragment_is_not_grafted_onto_a_finished_prose_round() {
  use crate::api::ApiError;
  let dir = scratch("failure-graft-prose");
  let path = dir.join("1.md");

  let mut session = template(&yolo_cfg(), &[]);
  session.input = "say something".into();
  session.promote_input();
  session.begin_round();
  session.push_text("here is a complete prose answer");
  session.close_turn();
  // A second turn starts streaming and the request dies.
  session.input = "now something else".into();
  session.promote_input();
  session.begin_round();
  session.push_text("I was partway");
  write_session(&path, &session);
  let snapshot = load(&path);

  // Mid-flight the user deletes the dangling turn, leaving the finished prose turn as the last one.
  let mut edited = load(&path);
  edited.turns.truncate(1);
  edited.close_turn();
  write_session(&path, &edited);

  daemon::record_provider_failure(
    &path,
    snapshot,
    &ApiError::Timeout(std::time::Duration::from_secs(300)),
  )
  .unwrap();

  let after = load(&path);
  assert_eq!(
    after
      .turns
      .last()
      .unwrap()
      .rounds
      .last()
      .unwrap()
      .assistant
      .text
      .as_deref(),
    Some("here is a complete prose answer"),
    "a finished prose answer must not be replaced by an aborted fragment"
  );
}

/// The failure note is written by loading a transcript that may be mid-stream, with `text` still
/// open. A note rendered inside that open region is swallowed by the parser on the next read --
/// taking the input region and the user's typed message with it, and wedging the file for good.
#[test]
fn a_failure_note_on_a_half_written_turn_does_not_corrupt_the_file() {
  use crate::api::ApiError;
  let dir = scratch("failure-midstream");
  let path = dir.join("1.md");
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "do the thing".into();
  session.promote_input();
  session.begin_round();
  // Push text without closing the section: this is precisely the state a live stream leaves on
  // disk, because `push_text` opens it and only `finish_section` closes it.
  session.push_text("I was partway through answering");
  write_session(&path, &session);
  session = load(&path);
  assert_eq!(
    session.section(),
    Some("text"),
    "mid-stream: the section is open so a later run can resume mid-sentence"
  );

  daemon::record_provider_failure(
    &path,
    load(&path),
    &ApiError::Timeout(std::time::Duration::from_secs(300)),
  )
  .unwrap();

  let reloaded = load(&path);
  assert!(
    reloaded
      .turns
      .last()
      .unwrap()
      .notes
      .iter()
      .any(|n| n.text.contains("timed out")),
    "note must be readable as a note, not swallowed as assistant text"
  );
  assert_eq!(
    reloaded.turns[0].rounds[0].assistant.text.as_deref(),
    Some("I was partway through answering"),
    "the partial answer must not be corrupted by the note"
  );
  assert_eq!(
    reloaded.section(),
    None,
    "the dangling section is closed: {reloaded:?}"
  );
  assert!(
    reloaded.turn_in_progress(),
    "and the turn is still resumable"
  );
}

/// A harness note renders immediately before the input region and both are `user` role. Strict
/// OpenAI-compatible routers reject two consecutive same-role messages, and that rejection is a
/// terminal 400 -- so it would park the session permanently over note placement.
#[test]
fn a_harness_note_and_the_input_do_not_come_out_as_two_user_messages() {
  let mut s = template(&yolo_cfg(), &[]);
  s.input = "do the next thing".into();
  s.promote_input();
  s.begin_round();
  s.push_text("working on it");
  s.close_turn();
  s.input = "carry on from there".into();
  s.add_note(
    session::NoteBy::Harness,
    "[harness] the provider refused the last request; re-sending is safe.",
  );

  let msgs = s.request_messages();
  for pair in msgs.windows(2) {
    assert_ne!(
      pair[0].role, pair[1].role,
      "two consecutive same-role messages: {msgs:?}"
    );
  }
  // And both halves of the instruction still reach the model.
  assert!(
    msgs.iter().any(|m| m
      .content
      .as_deref()
      .is_some_and(|c| c.contains("provider refused") && c.contains("carry on"))),
    "note and input must arrive together: {msgs:?}"
  );
}

/// `sf stop` has to stick. Un-parking re-arms a session so an edit can retry it -- and without
/// this, editing a stopped session's file for any reason would quietly restart it.
#[test]
fn a_stopped_session_is_not_rearmed_by_unparking() {
  let name = "stop-test";
  control::update(name, |s| {
    s.arm();
    s.parked = true;
    s.blocked = true;
  });
  control::update(name, |s| s.disarm());
  assert!(control::snapshot(name).stopped, "`sf stop` is recorded");

  // This is what the sweep's un-park branch does.
  control::update(name, |s| {
    if !s.stopped {
      s.arm();
    } else {
      s.parked = false;
      s.blocked = false;
    }
  });
  let after = control::snapshot(name);
  assert!(
    !after.armed,
    "a stopped session must stay stopped after an edit"
  );
  assert!(
    !after.parked && !after.blocked,
    "but the park itself is cleared"
  );

  // An explicit send is how the user changes their mind, and it works.
  control::update(name, |s| s.arm());
  assert!(control::snapshot(name).armed, "`sf send` re-arms it");
  assert!(!control::snapshot(name).stopped);
}

/// The bands are advice the model can actually follow. Derived from the window rather than the
/// configured ceiling, because `output_allowance` reserves up to 40k for the reply: at a 65k window
/// the prompt only gets 25536, so thresholds computed from the configured number would sit above
/// the point where the model gets truncated anyway.
#[test]
fn the_context_bands_are_always_reachable() {
  use crate::tokens::context_bands;
  for budget in [
    16_000u32, 32_000, 65_536, 96_000, 131_072, 200_000, 512_000, 1_000_000,
  ] {
    let (lo, hi) = context_bands(budget);
    let usable = crate::tokens::prompt_budget(budget);
    assert!(
      lo <= hi,
      "the start-pruning mark is above the ceiling at {budget}: {lo} > {hi}"
    );
    assert!(
      hi <= usable,
      "the ceiling exceeds what the model actually gets at {budget}: {hi} > {usable}"
    );
  }
  // And it scales: a bigger window buys a bigger budget, not a fixed one.
  let (small_lo, small_hi) = context_bands(65_536);
  let (big_lo, big_hi) = context_bands(512_000);
  assert!(
    big_lo > small_lo && big_hi > small_hi,
    "bands must follow the window: {small_lo}/{small_hi} vs {big_lo}/{big_hi}"
  );
}

/// On by default, and off means the prompt carries nothing at all -- not a stripped paragraph, but
/// never written in the first place.
#[test]
fn the_context_size_note_is_on_by_default_and_absent_when_off() {
  use crate::prompt::context_size_note;
  let cfg = yolo_cfg();
  assert!(
    cfg.tell_model_context_size,
    "on unless the user says otherwise"
  );
  let on = context_size_note(&cfg, &Default::default());
  assert!(on.contains("ctx:N"), "{on}");
  assert!(
    on.contains("stale"),
    "the reading is a snapshot, and the model has to be told: {on}"
  );

  let mut off = yolo_cfg();
  off.tell_model_context_size = false;
  assert_eq!(
    context_size_note(&off, &Default::default()),
    "",
    "off means zero bytes, so there is nothing to strip back out"
  );

  // The fence overrides the app-wide setting either way.
  let session_on = crate::types::SessionConfig {
    tell_model_context_size: Some(true),
    ..Default::default()
  };
  assert!(context_size_note(&off, &session_on).contains("ctx:N"));
}

/// Written once into the transcript. Injected per request it would be re-appended to every earlier
/// result on every turn, so the file would grow without bound.
#[test]
fn a_tool_result_is_tagged_once_and_only_once() {
  use crate::session::ToolOutcome;
  let mut outcomes = vec![
    ToolOutcome {
      id: "a".into(),
      name: "read".into(),
      output: "file body".into(),
      ok: true,
      ms: 1,
    },
    ToolOutcome {
      id: "b".into(),
      name: "bash".into(),
      output: "line\n".into(),
      ok: true,
      ms: 2,
    },
  ];
  crate::flow::annotate_context_size(&mut outcomes, Some(45454));
  assert!(
    outcomes[0].output.contains("[ctx:45454]"),
    "{}",
    outcomes[0].output
  );
  assert!(
    outcomes[1].output.contains("[ctx:45454]"),
    "{}",
    outcomes[1].output
  );

  // Nothing on the first turn, where there is no measured count to report.
  let mut blind = vec![ToolOutcome {
    id: "c".into(),
    name: "read".into(),
    output: "body".into(),
    ok: true,
    ms: 0,
  }];
  crate::flow::annotate_context_size(&mut blind, None);
  assert_eq!(blind[0].output, "body", "no invented number");
}

/// The tag goes in the transcript, so it comes back on later turns without being rebuilt -- and the
/// calibration that measures the payload sees exactly the bytes that go on the wire.
#[test]
fn the_tag_survives_into_the_next_request_without_growing() {
  use crate::session::ToolOutcome;
  let dir = scratch("ctx-tag");
  let path = dir.join("1.md");
  let mut session = template(&yolo_cfg(), &[]);
  session.input = "go".into();
  session.promote_input();
  session.begin_round();
  session.push_text("calling a tool");
  write_session(&path, &session);

  let mut outcomes = vec![ToolOutcome {
    id: "t1".into(),
    name: "read".into(),
    output: "body".into(),
    ok: true,
    ms: 1,
  }];
  crate::flow::annotate_context_size(&mut outcomes, Some(12345));
  session.finish_round(vec![], outcomes);
  write_session(&path, &session);

  // Two requests built from the same file must carry the tag the same number of times: rebuilding
  // must not re-append it to the earlier result.
  let a = load(&path).request_messages();
  let b = load(&path).request_messages();
  let count = |m: &[crate::types::ChatMessage]| {
    m.iter()
      .filter(|x| x.content.as_deref().is_some_and(|c| c.contains("[ctx:")))
      .count()
  };
  assert_eq!(count(&a), 1, "the tag is written once into the file");
  assert_eq!(count(&a), count(&b), "and rebuilding must not add another");
}

/// A truncated reply is exactly the shape with more than one think block. If the second one is
/// not consumed, "visible" output comes back non-empty when the model never actually said
/// anything -- and the turn gets blamed on a transcript that was never full.
#[test]
fn visible_text_strips_every_think_block() {
  use crate::daemon::visible_text;
  assert_eq!(visible_text("<think>ab</think>answer"), "answer");
  assert_eq!(
    visible_text("<think>a</think>mid<think>b</think>end"),
    "midend"
  );
  assert_eq!(
    visible_text("<think>ab<think>cd"),
    "",
    "an unclosed think tag means nothing was visible"
  );
  assert_eq!(visible_text("no tags at all"), "no tags at all");
  assert_eq!(visible_text(""), "");
  // Same input the writer splits across regions, so the two agree.
  assert_eq!(visible_text("<think>reason</think>"), "");
}

/// A refusal used to cost five requests before parking, and against a spent daily quota that is
/// five more the user does not have. `armed` cannot express "stop" on its own, because yolo mode
/// re-drives anything with work outstanding, so the block has to be its own flag.
#[test]
fn a_send_clears_a_provider_block_and_nothing_else_does() {
  let name = "block-test";
  control::update(name, |s| s.blocked = true);
  assert!(
    control::snapshot(name).blocked,
    "set by a terminal provider error"
  );

  control::update(name, |s| s.blocked = false);
  assert!(
    !control::snapshot(name).blocked,
    "cleared by the sweep, not by time"
  );

  control::update(name, |s| s.blocked = true);
  control::update(name, |s| s.arm());
  assert!(
    !control::snapshot(name).blocked,
    "`sf send` is the user saying try again, so it must clear a refusal"
  );
}

/// A typo is the one failure a user can fix instantly, and the one least likely to be guessed at:
/// the file just stops.
#[test]
fn a_misspelled_provider_is_reported_in_the_transcript() {
  let dir = scratch("typo-provider");
  let path = dir.join("1.md");
  let mut cfg = yolo_cfg();
  cfg.providers.insert(
    "openrouter".to_string(),
    crate::types::Provider {
      base_url: "https://openrouter.ai/api".into(),
      headers: Default::default(),
      llama: false,
      reasoning_style: Default::default(),
    },
  );
  let mut session = template(&cfg, &[]);
  session.input = "hi".into();
  session.promote_input();
  session.config.provider = Some("openroter".into());
  write_session(&path, &session);

  let client = crate::api::LlamaClient::new(&cfg);
  let err = client
    .for_provider(&cfg, Some("openroter"), &Default::default())
    .err()
    .expect("unknown provider must be an error, not a silent fallback");
  daemon::record_config_failure(&path, &err).unwrap();

  let reloaded = load(&path);
  let notes = &reloaded.turns.last().unwrap().notes;
  assert!(
    notes.iter().any(|n| n.text.contains("openroter")),
    "the note must name what was wrong: {notes:?}"
  );
  assert!(
    notes
      .iter()
      .any(|n| n.text.contains("Re-sending alone will fail")),
    "and must not send the user in circles: {notes:?}"
  );
}

/// A timeout is not a refusal. Telling the user "re-sending is safe" when a stream died
/// mid-generation is advice that can be wrong, and the two need different wording.
#[test]
fn a_transport_failure_does_not_get_refusal_wording() {
  use crate::api::ApiError;
  let dir = scratch("timeout-note");
  let path = dir.join("1.md");
  write_session(&path, &template(&yolo_cfg(), &[]));

  daemon::record_provider_failure(
    &path,
    load(&path),
    &ApiError::Timeout(std::time::Duration::from_secs(300)),
  )
  .unwrap();
  let text = read_to_string(&path).unwrap();
  assert!(text.contains("left half-written"), "{text}");
  assert!(
    !text.contains("this turn was never answered"),
    "a timeout is not a refusal: {text}"
  );

  // And the two must not overwrite each other into one misleading note.
  daemon::record_provider_failure(
    &path,
    load(&path),
    &ApiError::NonOkStatus(429, r#"{"error":{"message":"nope"}}"#.to_string(), false),
  )
  .unwrap();
  let text = read_to_string(&path).unwrap();
  assert!(text.contains("this turn was never answered"), "{text}");
}

#[test]
fn the_burst_cap_is_reported_in_the_transcript_not_silently() {
  // Going quiet looks identical to "the model finished", which leaves the user guessing why
  // nothing happens until they send again by hand.
  // There is no fixed ceiling any more -- `burst_max_turns` defaults to 0, meaning unlimited --
  // but when one *is* set, the transcript has to say so rather than going quiet.
  assert_eq!(yolo_cfg().burst_max_turns, 0, "no ceiling unless asked for");
  // The daemon's own text, not a copy: a hand-written duplicate passes no matter what the daemon
  // actually says, which is how this drifted out of date without anyone noticing.
  let cap = 20;
  let note = daemon::burst_ceiling_note(cap);
  let mut s = Session {
    system: "sys".into(),
    ..Default::default()
  };
  s.add_note(session::NoteBy::Harness, &note);
  let rendered = s.render();
  assert!(rendered.contains("hit its turn ceiling"), "{rendered}");
  assert!(
    rendered.contains("set `burst_max_turns = 0`"),
    "the user has to be told how to turn it off: {rendered}"
  );
  // and it must come back as a message, so the model actually reads it next turn
  let msgs = s.request_messages();
  assert!(
    msgs.iter().any(|m| m
      .content
      .as_deref()
      .is_some_and(|c| c.contains("hit its turn ceiling"))),
    "the note must reach the model: {msgs:?}"
  );
}

// ---- model-invented roles (the paper's Figure 3b behavior)

#[test]
fn a_role_the_model_invents_is_kept_and_handed_back() {
  // Exactly what the model tried: its own `notes` region, plus a `text` it put straight into a
  // turn. The invented role must survive; the misplaced one must still be refused, but with an
  // error that states the grammar instead of leaving the model to guess.
  let raw = "\
```toml
model = \"qwen3.8-27b\"
```
<!-- slopfile:begin -->
<!-- slopfile:system -->
sys
<!-- /slopfile -->
<!-- slopfile:turn -->
<!-- slopfile:user -->
source the mods
<!-- /slopfile -->
<!-- slopfile:notes -->
7 repos verified. Search running in background.
<!-- /slopfile -->
<!-- slopfile:assistant -->
<!-- slopfile:scratchpad -->
next: clone with --depth 1
<!-- /slopfile -->
<!-- /slopfile -->
<!-- slopfile:input -->
";
  let p = scratch("custom-role").join("s.md");
  std::fs::write(&p, raw).unwrap();
  let s = transcript::load(&p).expect("an invented role must not be a parse error");
  assert_eq!(s.turns[0].custom.len(), 1);
  assert_eq!(s.turns[0].custom[0].0, "notes");
  assert!(s.turns[0].custom[0].1.contains("7 repos verified"));
  assert_eq!(s.turns[0].rounds[0].assistant.custom[0].0, "scratchpad");

  // Round-trips, so the model finds its own role still there (the fence gains the harness's
  // default `thinking`, which is expected and unrelated).
  let again = s.render();
  assert!(
    again.contains(
      "<!-- slopfile:notes -->\n7 repos verified. Search running in background.\n<!-- /slopfile -->"
    ),
    "{again}"
  );
  assert!(
    again.contains("<!-- slopfile:scratchpad -->\nnext: clone with --depth 1\n<!-- /slopfile -->"),
    "{again}"
  );
  let (reparsed, report) = session::parse(&again);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(reparsed.turns[0].custom[0].0, "notes", "{again}");

  // And comes back as context under its own name.
  let msgs = s.request_messages();
  assert!(
    msgs.iter().any(|m| m
      .content
      .as_deref()
      .is_some_and(|c| c.contains("7 repos verified"))),
    "the invented role must reach the model: {msgs:?}"
  );
}

#[test]
fn a_misplaced_builtin_says_where_it_goes() {
  let raw = "\
```toml
model = \"qwen3.8-27b\"
```
<!-- slopfile:begin -->
<!-- slopfile:system -->
sys
<!-- /slopfile -->
<!-- slopfile:turn -->
<!-- slopfile:text -->
a reply, but not inside an assistant
<!-- /slopfile -->
<!-- /slopfile -->
<!-- slopfile:input -->
";
  let p = scratch("misplaced").join("s.md");
  std::fs::write(&p, raw).unwrap();
  let err = transcript::load(&p).expect_err("text straight into a turn is still wrong");
  assert!(
    err.contains("assistant"),
    "the error must say where `text` belongs: {err}"
  );
  assert!(
    err.contains("invent"),
    "the error must say invented roles are allowed: {err}"
  );
}

#[test]
fn a_fence_that_is_valid_toml_but_names_an_unknown_value_is_not_rewritten() {
  // The sibling of `a_hand_edited_fence_survives_a_save_even_behind_prose`: that one covers TOML
  // that does not parse at all, this one covers TOML that parses but carries a value we reject.
  // The second is the sharper edge, because `thinking = "meduim"` is well-formed TOML, so the
  // "leave the fence alone" branch that catches the first case does not fire on its own.
  let content = "\
```toml
model = \"special-model\"
thinking = \"meduim\"
```
<!-- slopfile:begin -->
<!-- slopfile:system -->
sys
<!-- /slopfile -->
<!-- slopfile:input -->
";
  let p = scratch("fence-bad-value").join("s.md");
  std::fs::write(&p, content).unwrap();
  let s = transcript::load(&p).expect("a bad fence value is a warning, not a refusal");
  let out = s.render();
  assert!(
    out.contains("thinking = \"meduim\""),
    "the rejected value was silently rewritten to our default:\n{out}"
  );
  assert!(
    out.contains("model = \"special-model\""),
    "the other settings were lost:\n{out}"
  );
}
