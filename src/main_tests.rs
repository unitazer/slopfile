use std::{env::temp_dir, fs::read_to_string, thread::spawn};

use super::*;
use crate::daemon::{next_action, Action};
use crate::docker::restart_llama_server;
use crate::flow::{
  questions_resolvable, resolve_questions, session_template, spawn_subagent, subagent_depth,
  write_tool_blocks,
};
use crate::lex::{CONTEXT_FULL_NOTE, REPORTED_MARKER};
use crate::stream::StreamWriter;
use crate::transcript::{draft_tail, tail_user_text};
use crate::types::{AppConfig, Confirmations, Role};

fn yolo_cfg() -> AppConfig {
    AppConfig {
      confirmations: Confirmations::Yolo,
      api_base: "http://127.0.0.1:8080".into(),
      workdir: "/tmp".into(),
      transcripts_dir: "/tmp".into(),
      skills_dir: "/tmp/does-not-exist".into(),
      sweep_interval_ms: 250,
      model: "qwen3.8-27b".into(),
      max_context_tokens: 8192,
      tool_timeout_ms: 30000,
      abort_restart_server: false,
      llama_container: "test-llama".into(),
      subagent_max_depth: 2,
      stream_idle_timeout_secs: 300,
    }
  }

  fn sample_with_question_block() -> String {
    let block = questions::render_block(&[questions::Question {
      id: 1,
      question: "red or blue?".into(),
      answers: vec!["red".into(), "blue".into()],
      default: None,
    }]);
    format!(
      "```toml\nmodel = \"qwen3.8-27b\"\n```\n# Sm\nSystem text.\n\n### You\nhi\n\n### Assistant\n<!-- thinking -->\nask\n<!-- /thinking -->\nlet me ask\n<!-- tool:question {{\"desc\":\"x\",\"data\":[{{\"question\":\"red or blue?\",\"answers\":[\"red\",\"blue\"]}}]}} -->\n<!-- result:question ok=ask -->\n\n{block}<!-- q:1 {{\"question\":\"red or blue?\",\"answers\":[\"red\",\"blue\"]}} -->\n<!-- call:question {{\"desc\":\"x\",\"data\":[{{\"question\":\"red or blue?\",\"answers\":[\"red\",\"blue\"]}}]}} -->\n"
    )
  }

  #[test]
  fn cli_abort_appends_marker_and_is_idempotent() {
    let dir = temp_dir().join("slopfile-test-cliabort");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n").unwrap();
    cmd_abort(path.to_str().unwrap());
    let one = read_to_string(&path).unwrap();
    assert!(one.ends_with("<!-- slopfile:abort -->\n"), "marker appended at end");
    assert_eq!(one.matches(transcript::ABORT_MARKER).count(), 1);
    cmd_abort(path.to_str().unwrap());
    let two = read_to_string(&path).unwrap();
    assert_eq!(two, one, "second abort must not duplicate the marker");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn docker_restart_speaks_the_docker_api_over_a_unix_socket() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    let dir = temp_dir().join("slopfile-test-dockersock");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("docker.sock");
    let _ = std::fs::remove_file(&sock);

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
      c.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").unwrap();
    });

    let path = sock.to_str().unwrap();
    assert!(restart_llama_server(path, "clanker-llama").is_ok(), "204 -> ok");
    server.join().unwrap();

    let sock2 = sock.with_extension("sock2");
    let _ = std::fs::remove_file(&sock2);
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
      c.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").unwrap();
    });
    let p2 = path2.to_str().unwrap();
    let err = restart_llama_server(p2, "ghost").unwrap_err();
    assert!(err.contains("404"), "error must surface the status line: {err}");
    server2.join().unwrap();
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn yolo_resolves_questions_with_first_option() {
    let dir = temp_dir().join("slopfile-test-yolo");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, sample_with_question_block()).unwrap();

    let text = read_to_string(&path).unwrap();
    let pf = transcript::parse_file(&text);
    assert!(!pf.pending_questions.is_empty());
    let cfg = yolo_cfg();
    assert!(questions_resolvable(&pf, &cfg), "yolo is always resolvable");
    resolve_questions(&path, &text, &pf, &cfg).unwrap();

    let after = read_to_string(&path).unwrap();
    assert!(!after.contains("QUESTIONS"), "block removed");
    assert!(!after.contains("<!-- q:"), "stashes removed");
    assert!(!after.contains("<!-- call:"), "call stash removed");
    assert!(after.contains("<!-- tool:question"), "tool block present");
    assert!(after.contains("\"answer\":\"red\""), "first option used");
    assert!(after.contains("<!-- result:question ok=ask"), "result closed");

    let pf2 = transcript::parse_file(&after);
    let roles: Vec<Role> = pf2.history.iter().map(|m| m.role).collect();
    assert_eq!(*roles.last().unwrap(), Role::Tool);
    assert!(next_action(&pf2, &after, &cfg, false) != Action::Idle, "daemon continues after yolo");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn ask_mode_is_not_resolvable_without_answers() {
    let dir = temp_dir().join("slopfile-test-ask");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, sample_with_question_block()).unwrap();

    let text = read_to_string(&path).unwrap();
    let pf = transcript::parse_file(&text);
    let cfg = AppConfig { confirmations: Confirmations::Ask, ..yolo_cfg() };
    assert!(!questions_resolvable(&pf, &cfg));
    assert!(next_action(&pf, &text, &cfg, false) == Action::Idle, "ask mode waits, no work");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn ask_mode_unanswered_leading_block_stalls_later_answered_block() {
    let dir = temp_dir().join("slopfile-test-ask-order");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");

    let block_a = "QUESTIONS\n| # | Question | Options |\n| 1 | red or blue | red, blue |\nANSWER: (numbers or text, comma separated)\n>\n";
    let block_b = "QUESTIONS\n| # | Question | Options |\n| 2 | how long? | 3, 5, 10 |\nANSWER: (numbers or text, comma separated)\n> \nblue\n";
    let text = format!(
      "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n\n### Assistant\n<!-- @done -->\n\n{block_a}<!-- q:1 {{\"question\":\"red or blue?\",\"answers\":[\"red\",\"blue\"]}} -->\n<!-- call:question {{\"data\":[{{\"question\":\"red or blue?\",\"answers\":[\"red\",\"blue\"]}}]}} -->\n\n{block_b}<!-- q:2 {{\"question\":\"how long?\",\"answers\":[\"3\",\"5\",\"10\"]}} -->\n<!-- call:question {{\"data\":[{{\"question\":\"how long?\",\"answers\":[\"3\",\"5\",\"10\"]}}]}} -->\n"
    );
    std::fs::write(&path, &text).unwrap();

    let pf = transcript::parse_file(&text);
    assert_eq!(pf.pending_questions.len(), 2, "two pending blocks parsed");
    let cfg = AppConfig { confirmations: Confirmations::Ask, ..yolo_cfg() };
    assert!(
      !questions_resolvable(&pf, &cfg),
      "leading unanswered block must gate resolvability"
    );

    resolve_questions(&path, &text, &pf, &cfg).unwrap();
    let after = read_to_string(&path).unwrap();
    assert!(after.contains("QUESTIONS"), "block A stays pending");
    assert!(after.contains("\nblue\n"), "block B answer not consumed");
    assert!(!after.contains("<!-- tool:question"), "no block resolved");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn abort_marker_is_stripped_but_rest_preserved() {
    let dir = temp_dir().join("slopfile-test-abort");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    let src = format!(
      "{}\n\n<!-- slopfile:abort -->\n",
      sample_with_question_block()
    );
    std::fs::write(&path, src.clone()).unwrap();
    transcript::strip_abort_marker(&path, &src).unwrap();
    let after = read_to_string(&path).unwrap();
    assert!(!after.contains("slopfile:abort"));
    assert!(after.contains("### You"));
    assert!(after.contains("QUESTIONS"), "user block untouched");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn prose_mentioning_abort_marker_is_not_an_abort() {
    let prose = "\
```toml
model = \"qwen3.8-27b\"
```
# S
Sys.

### You
hi

### Assistant
Let me read a file.
<!-- tool:read {\"path\":\"PLAN.md\"} -->
47: - Abort: `<!-- slopfile:abort -->` marker line appended by `:Abort`
<!-- result:read ok=0ms -->
";
    assert!(!transcript::abort_marker_present(prose), "prose mention must not abort");
    assert!(transcript::abort_marker_present("hi\n<!-- slopfile:abort -->\n"));
    assert!(transcript::abort_marker_present(
      "<!-- slopfile:live {\"phase\":\"generating\"} --><!-- slopfile:abort -->\n"
    ));
    let pf = transcript::parse_file(prose);
    assert!(next_action(&pf, prose, &yolo_cfg(), false) != Action::Idle, "prose mention must not stall the file");
  }

  #[test]
  fn stream_writer_finalizes_sections_and_keeps_live_line() {
    let dir = temp_dir().join("slopfile-test-streamwriter");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(
      &path,
      "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n",
    )
    .unwrap();

    let live = "<!-- slopfile:live {\"phase\":\"generating\"} -->".to_string();
    let mut w = StreamWriter::new(
      &path,
      &read_to_string(&path).unwrap(),
      Some(live.clone()),
    );
    assert!(w.handle(&types::StreamEvent::Text("hello ".into())));
    assert!(w.handle(&types::StreamEvent::Text("world".into())));
    w.finish_section();
    let after = read_to_string(&path).unwrap();
    assert!(after.contains("hello world"), "streamed text kept: {after}");
    assert!(after.trim_end().ends_with(&live), "live line restored at bottom");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn stream_translates_raw_think_tags_and_drops_orphans() {
    let dir = temp_dir().join("slopfile-test-thinktags");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    let seed = "```toml\nmodel = \"m\"\n```\n# S\nSys.\n\n### You\ngo\n";
    std::fs::write(&path, seed).unwrap();

    let mut w = StreamWriter::new(&path, &read_to_string(&path).unwrap(), None);
    assert!(w.handle(&types::StreamEvent::Text("</think>answer text".into())));
    assert!(w.handle(&types::StreamEvent::Reasoning("hmm".into())));
    assert!(w.handle(&types::StreamEvent::Text("<think>ignored open".into())));
    w.finish_section();

    let after = read_to_string(&path).unwrap();
    assert!(!after.contains("</think>") && !after.contains("<think>"), "raw tags gone: {after}");
    assert!(after.contains("<!-- /thinking -->"), "orphan close became marker");
    assert!(after.contains("answer text"), "{after}");
    assert!(after.contains("<!-- thinking -->\nhmm"), "reasoning kept: {after}");
    assert!(after.contains("ignored open"), "{after}");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn finalize_turn_appends_marker_and_strips_abort_lines() {
    let dir = temp_dir().join("slopfile-test-finalize");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(
      &path,
      "### Assistant\npartial reply\n<!-- slopfile:abort -->\nlive line below\n",
    )
    .unwrap();
    daemon::finalize_turn(&path, "<!-- @done -->\n".into()).unwrap();
    let after = read_to_string(&path).unwrap();
    assert!(!after.contains("slopfile:abort"), "abort lines stripped");
    assert!(after.contains("partial reply"));
    assert!(after.contains("<!-- @done -->"));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn file_has_abort_drives_keep_going_unnverted() {
    let dir = temp_dir().join("slopfile-test-keepgoing-wiring");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, "### You\nhi\n").unwrap();

    assert!(!transcript::file_has_abort(&path), "marker-free file must keep going");
    let keep_going = || !transcript::file_has_abort(&path);
    assert!(keep_going(), "no marker -> keep_going() true (continue)");

    let mut f = read_to_string(&path).unwrap();
    f.push_str(transcript::ABORT_MARKER);
    f.push('\n');
    std::fs::write(&path, f).unwrap();

    assert!(transcript::file_has_abort(&path), "marker present -> has abort");
    assert!(!keep_going(), "marker present -> keep_going() false (abort)");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn skip_resolves_with_defaults() {
    let dir = temp_dir().join("slopfile-test-skip");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    let mut src = sample_with_question_block();
    src = src.replace("> \n", "> /skip\n");
    std::fs::write(&path, &src).unwrap();

    let text = read_to_string(&path).unwrap();
    let pf = transcript::parse_file(&text);
    assert!(!pf.pending_questions.is_empty());
    let cfg = AppConfig { confirmations: Confirmations::Ask, ..yolo_cfg() };
    assert!(questions_resolvable(&pf, &cfg), "/skip resolves in ask mode");
    resolve_questions(&path, &text, &pf, &cfg).unwrap();

    let after = read_to_string(&path).unwrap();
    assert!(after.contains("<!-- tool:question"), "resolved to tool block");
    assert!(!after.contains("/skip"), "command line removed");
    assert!(!after.contains("QUESTIONS"), "block removed");
    let pf2 = transcript::parse_file(&after);
    assert_eq!(pf2.history.last().map(|m| m.role), Some(Role::Tool));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn draft_below_done_is_work() {
    let dir = temp_dir().join("slopfile-test-draft");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, format!("{}\n<!-- @done -->\nkeep running daemons\n", sample_with_question_block())).unwrap();
    let text = read_to_string(&path).unwrap();
    let pf = transcript::parse_file(&text);
    assert!(next_action(&pf, &text, &yolo_cfg(), false) != Action::Idle);
    assert_eq!(draft_tail(&text).map(|s| s.trim().to_string()), Some("keep running daemons".into()));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn ask_mode_draft_waits_for_send_marker() {
    let base = "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n\n### Assistant\n<!-- @done -->\n";
    let draft = format!("{base}keep the daemons running\n");
    let cfg = AppConfig { confirmations: Confirmations::Ask, ..yolo_cfg() };

    let pf = transcript::parse_file(&draft);
    assert!(
      next_action(&pf, &draft, &cfg, false) == Action::Idle,
      "a plain save must not launch the model"
    );

    let sent = format!("{draft}{}\n", transcript::SEND_MARKER);
    let pf2 = transcript::parse_file(&sent);
    assert!(next_action(&pf2, &sent, &cfg, true) != Action::Idle, "a :Send arms the file");
    assert!(transcript::send_marker_present(&sent));
    assert!(
      draft_tail(&sent).is_some_and(|d| d.contains("keep the daemons running")),
      "the marker must never become the draft"
    );

    let stripped = transcript::without_send_lines(&sent);
    assert!(!transcript::send_marker_present(&stripped), "daemon consumes the marker");
    assert!(stripped.contains("keep the daemons running"), "draft kept");
  }

  #[test]
  fn context_full_note_is_daemon_owned_and_never_a_draft() {
    let base = "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n\n### Assistant\npartial reply here\n<!-- @done -->\n";
    let note = format!("{CONTEXT_FULL_NOTE}\n");

    let note_only = format!("{base}{note}");
    let text = &note_only;
    let pf = transcript::parse_file(text);
    assert!(draft_tail(text).is_none(), "note is not a draft");
    assert!(tail_user_text(text).is_none(), "note is not a section");
    assert!(next_action(&pf, text, &yolo_cfg(), false) == Action::Idle, "note-only tail must be idle");
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("context-full"))));

    let note_and_draft = format!("{base}{note}\ncontinue the work\n");
    let text = &note_and_draft;
    let pf = transcript::parse_file(text);
    assert_eq!(
      draft_tail(text).map(|s| s.trim().to_string()),
      Some("continue the work".into())
    );
    assert!(next_action(&pf, text, &yolo_cfg(), false) != Action::Idle);
  }

  #[test]
  fn promote_draft_skips_daemon_owned_note() {
    let dir = temp_dir().join("slopfile-test-promote-note");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    let text = format!(
      "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n\n### Assistant\npartial reply\n<!-- @done -->\n{CONTEXT_FULL_NOTE}\n\nnext message\n"
    );
    std::fs::write(&path, &text).unwrap();
    let _pf = transcript::parse_file(&text);
    crate::flow::promote_draft(&path, &text).expect("promote");
    let after = read_to_string(&path).unwrap();
    let note_ix = after.find(CONTEXT_FULL_NOTE).expect("note kept");
    let you_ix = after.find("### You\nnext message").expect("section right before draft");
    let done_ix = after.find("<!-- @done -->").expect("done marker");
    assert!(done_ix < note_ix && note_ix < you_ix, "layout: {after}");
    let section = tail_user_text(&after).expect("promoted section");
    let msgs = transcript::tail_sections(&section);
    assert!(msgs
      .iter()
      .any(|m| m.role == Role::User && m.content.as_deref() == Some("next message")));
    assert!(!section.contains("context-full"), "note must stay out of the section");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn session_template_has_default_header_and_prompt() {
    let cfg = yolo_cfg();
    let tpl = session_template(&cfg);
    assert!(tpl.contains("```toml\n"), "header fence: {tpl}");
    assert!(tpl.contains("model = \"qwen3.8-27b\""), "default model: {tpl}");
    assert!(tpl.contains("tools = [\"read\", \"write\", \"edit\", \"bash\", \"grep\", \"find\", \"ls\", \"question\", \"subagent\"]"));
    assert!(!tpl.contains("solve"), "solve is campaign-only: {tpl}");
    assert!(tpl.contains("# Session\n"), "session heading: {tpl}");
    assert!(tpl.contains("You are an expert software engineering assistant"));
    assert!(!tpl.contains("This session is a Markdown transcript file"));
    assert!(!tpl.contains("<!-- @done -->"));
    assert!(!tpl.contains("### Assistant"));
    assert!(tpl.contains("- read: Read files as numbered lines"));
    assert!(tpl.contains("Use read to examine files before editing"));
    assert!(tpl.contains("Current working directory: /tmp"));
    assert!(tpl.ends_with("### You\n"));
    let pf = transcript::parse_file(&tpl);
    assert_eq!(pf.history.len(), 1, "system message only");
    assert_eq!(pf.history[0].role, Role::System);
    let sp = pf.history[0].content.as_deref().unwrap();
    assert!(sp.contains("expert software engineering assistant"));
    assert!(!sp.contains("# Session"), "heading stripped from prompt");
  }

  #[test]
  fn session_template_lists_skills() {
    let dir = temp_dir().join("slopfile-test-skills");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
      dir.join("b-deploy.md"),
      "# Deploy checklist\n\nAlways run the tests before shipping.\n\nDetails here.\n",
    )
    .unwrap();
    std::fs::write(dir.join("a-style.md"), "# Style guide\nUse tabs.\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "ignored, not markdown\n").unwrap();

    let mut cfg = yolo_cfg();
    cfg.skills_dir = dir.to_string_lossy().into();
    let tpl = session_template(&cfg);
    assert!(tpl.contains("Skills available in"), "index missing: {tpl}");
    assert!(tpl.contains("- a-style.md: Use tabs"), "{tpl}");
    assert!(tpl.contains("- b-deploy.md: Always run the tests before shipping."), "{tpl}");
    assert!(!tpl.contains("notes.txt"), "non-md must be skipped: {tpl}");
    std::fs::write(
      dir.join("c-front.md"),
      "---\nname: front-skill\ndescription: >\n  Handles frontmatter\n  skills properly.\nmetadata:\n  x: 1\n---\n\n# Body ignored\n",
    )
    .unwrap();
    let tpl2 = session_template(&cfg);
    assert!(
      tpl2.contains("- c-front.md: Handles frontmatter skills properly."),
      "frontmatter desc missing: {tpl2}"
    );
    std::fs::write(
      dir.join("d-manual.md"),
      "---\nname: manual-only\ndescription: Only for explicit requests.\ndisable-model-invocation: true\n---\n\n# Manual\n",
    )
    .unwrap();
    let tpl3 = session_template(&cfg);
    assert!(!tpl3.contains("d-manual.md"), "disabled skill must be skipped: {tpl3}");
    assert!(tpl3.contains("- c-front.md:"), "other skills still listed: {tpl3}");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn subagent_depth_and_spawn_filtering() {
    assert_eq!(subagent_depth("session"), 0);
    assert_eq!(subagent_depth("session.sub-01"), 1);
    assert_eq!(subagent_depth("session.sub-01.sub-02"), 2);

    let dir = temp_dir().join("slopfile-test-spawn");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = yolo_cfg();
    cfg.subagent_max_depth = 2;
    let parent = dir.join("session.md");
    std::fs::write(&parent, "x\n").unwrap();

    let c1 = spawn_subagent(&parent, "do task one", &cfg).unwrap();
    assert_eq!(c1.file_name().unwrap(), "session.sub-01.md");
    let t1 = read_to_string(&c1).unwrap();
    assert!(t1.contains("tools_excluded = [\"question\"]"), "{t1}");
    assert!(t1.ends_with("### You\ndo task one\n"), "{t1}");

    let c2 = spawn_subagent(&c1, "nested task", &cfg).unwrap();
    assert_eq!(c2.file_name().unwrap(), "session.sub-01.sub-01.md");
    let t2 = read_to_string(&c2).unwrap();
    assert!(
      t2.contains("tools_excluded = [\"question\", \"subagent\"]"),
      "depth-2 child loses subagent tool: {t2}"
    );
    assert!(
      t2.contains("nobody will answer") && !t2.contains("call the `question` tool"),
      "prompt adapts to missing question tool: {t2}"
    );
    assert!(
      !t2.contains("- subagent:") && t1.contains("- subagent:"),
      "prompt advertises only available tools"
    );

    let err = spawn_subagent(&c2, "too deep", &cfg).unwrap_err();
    assert!(err.contains("depth limit"), "{err}");

    let c1b = spawn_subagent(&parent, "second child", &cfg).unwrap();
    assert_eq!(c1b.file_name().unwrap(), "session.sub-02.md");

    std::fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test]
  async fn tool_blocks_spawn_subagents_and_enforce_exclusions() {
    let dir = temp_dir().join("slopfile-test-blocks");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = yolo_cfg();
    let parent = dir.join("s.md");
    let text = concat!(
      "```toml\nmodel = \"qwen3.8-27b\"\ntools_excluded = [\"bash\"]\n```\n",
      "# S\nSys.\n\n### You\nhi\n"
    );
    std::fs::write(&parent, text).unwrap();

    let pf = crate::transcript::parse_file(text);
    let bash_call = crate::types::ToolCall::new("1", "bash", "{\"command\":\"echo hi\"}");
    write_tool_blocks(&parent, &[bash_call], &cfg, &pf).await.unwrap();
    let after = read_to_string(&parent).unwrap();
    assert!(after.contains("tool is disabled"), "{after}");

    let sub_call =
      crate::types::ToolCall::new("2", "subagent", "{\"name\":\"w\",\"task\":\"do it\"}");
    write_tool_blocks(&parent, &[sub_call], &cfg, &pf).await.unwrap();
    let after2 = read_to_string(&parent).unwrap();
    assert!(after2.contains("spawned at"), "{after2}");
    assert!(dir.join("s.sub-01.md").exists());

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn unfinished_subagents_blocks_until_reported() {
    let dir = temp_dir().join("slopfile-test-wait");
    std::fs::create_dir_all(&dir).unwrap();
    let parent = dir.join("p.md");
    let running = dir.join("p.sub-01.md");
    let done = dir.join("p.sub-02.md");
    std::fs::write(&parent, "x\n").unwrap();
    std::fs::write(&running, format!("### Assistant\nworking\n{}\n", crate::lex::DONE_MARKER)).unwrap();
    std::fs::write(
      &done,
      format!("### Assistant\ndone\n{}\n{REPORTED_MARKER}\n", crate::lex::DONE_MARKER),
    )
    .unwrap();

    let waiting = crate::daemon::unfinished_subagents(&parent).unwrap();
    assert_eq!(waiting, vec![running.clone()]);

    std::fs::write(&running, format!("{}\n", REPORTED_MARKER)).unwrap();
    assert!(crate::daemon::unfinished_subagents(&parent).unwrap().is_empty());

    let lookalike = dir.join("p.sub-notes.md");
    let grandchild = dir.join("p.sub-01.sub-01.md");
    std::fs::write(&lookalike, "unrelated\n").unwrap();
    std::fs::write(&grandchild, "nested, not a direct child\n").unwrap();
    assert!(crate::daemon::unfinished_subagents(&parent).unwrap().is_empty());

    std::fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test]
  async fn tools_allowlist_and_solve_are_enforced_at_runtime() {
    let dir = temp_dir().join("slopfile-test-allow");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = yolo_cfg();
    let parent = dir.join("s.md");
    let text = concat!(
      "```toml\nmodel = \"qwen3.8-27b\"\ntools = [\"read\", \"bash\"]\n",
      "tools_excluded = [\"grep\"]\n```\n# S\nSys.\n\n### You\nhi\n"
    );
    std::fs::write(&parent, text).unwrap();
    let pf = crate::transcript::parse_file(text);

    for (id, name, args) in [
      ("1", "find", "{\"suffix\":\".rs\"}"),
      ("2", "solve", "{\"summary\":\"done\"}"),
      ("3", "grep", "{\"pattern\":\"x\"}"),
      ("4", "read", "{}"),
    ] {
      std::fs::write(&parent, text).unwrap();
      let call = crate::types::ToolCall::new(id, name, args);
      write_tool_blocks(&parent, &[call], &cfg, &pf).await.unwrap();
      let after = read_to_string(&parent).unwrap();
      match name {
        "read" => assert!(!after.contains("tool is disabled"), "{after}"),
        _ => assert!(after.contains("tool is disabled"), "{name}: {after}"),
      }
    }
    assert!(
      !read_to_string(&parent).unwrap().contains(crate::lex::SOLVED_MARKER),
      "solve must not terminate a normal session"
    );

    std::fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test]
  async fn deliver_subagent_result_injects_report_into_parent() {
    let dir = temp_dir().join("slopfile-test-deliver");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let parent = dir.join("p.md");
    std::fs::write(
      &parent,
      "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n",
    )
    .unwrap();
    let child = dir.join("p.sub-01.md");
    std::fs::write(
      &child,
      format!(
        "```toml\nmodel = \"qwen3.8-27b\"\ntools_excluded = [\"question\"]\n```\n# S\nSys.\n\n### You\ntask\n\n### Assistant\nthe full final report\n{}\n",
        crate::lex::DONE_MARKER
      ),
    )
    .unwrap();

    crate::daemon::deliver_subagent_result(&child, crate::daemon::SubagentReport::FromTranscript).await.unwrap();

    let child_text = read_to_string(&child).unwrap();
    assert!(child_text.contains(REPORTED_MARKER), "{child_text}");
    let parent_text = read_to_string(&parent).unwrap();
    assert!(parent_text.contains("[system] Subagent 'p.sub-01' finished."), "{parent_text}");
    assert!(parent_text.contains("the full final report"), "{parent_text}");
    assert!(parent_text.contains(crate::transcript::SEND_MARKER), "{parent_text}");

    let pf = crate::transcript::parse_file(&parent_text);
    assert_eq!(pf.history.len(), 2, "system + report turn: {:?}", pf.history);
    let last = pf.history.last().unwrap();
    assert_eq!(last.role, crate::types::Role::User);
    assert!(last.content.as_deref().is_some_and(|c| c.contains("the full final report")));

    crate::daemon::deliver_subagent_result(&child, crate::daemon::SubagentReport::FromTranscript).await.unwrap();
    let once_more = read_to_string(&parent).unwrap();
    assert_eq!(once_more.matches("the full final report").count(), 1, "no duplicate reports");

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn reported_marker_is_daemon_owned_and_keeps_child_idle() {
    use crate::lex::{lex, TokKind};
    let text = format!(
      "### Assistant\nfinal report\n{}\n{}\n",
      crate::lex::DONE_MARKER,
      REPORTED_MARKER
    );
    let kinds: Vec<TokKind> = lex(&text).into_iter().map(|t| t.kind).collect();
    assert!(kinds.contains(&TokKind::Reported), "marker lexed as Reported: {kinds:?}");
    assert!(
      lex(&text).iter().all(|t| !t.kind.is_daemon_owned() || t.kind == TokKind::Reported),
      "reported marker counts as daemon-owned"
    );
    assert!(tail_user_text(&text).is_none(), "reported marker is not queued user text");

    let cfg = yolo_cfg();
    let pf = crate::transcript::parse_file(&text);
    assert_eq!(next_action(&pf, &text, &cfg, false), Action::Idle);
  }

  #[test]
  fn init_refuses_nonempty_and_initializes_empty() {
    let dir = temp_dir().join("slopfile-test-init");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = yolo_cfg();

    let with_content = dir.join("busy.md");
    std::fs::write(&with_content, "user wrote this\n").unwrap();
    let err = init_template_for(&with_content.to_string_lossy(), &cfg);
    assert!(err.is_err(), "non-empty file must be refused");
    assert_eq!(std::fs::read_to_string(&with_content).unwrap(), "user wrote this\n");

    let empty = dir.join("fresh.md");
    std::fs::write(&empty, "").unwrap();
    let tpl = init_template_for(&empty.to_string_lossy(), &cfg).expect("empty file initializes");
    assert!(tpl.contains("### You"));
    assert_eq!(std::fs::read_to_string(&empty).unwrap(), "", "init_template_for must not write");

    let missing = dir.join("missing.md");
    let tpl2 = init_template_for(&missing.to_string_lossy(), &cfg).expect("missing file initializes");
    assert!(tpl2.contains("model = \"qwen3.8-27b\""));

    let reserved = dir.join("my.sub-notes.md");
    let err = init_template_for(&reserved.to_string_lossy(), &cfg).unwrap_err();
    assert!(err.contains(".sub-"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
  }
