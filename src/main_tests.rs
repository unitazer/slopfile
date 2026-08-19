  use std::{env::temp_dir, fs::read_to_string, thread::spawn};

use super::*;
use crate::types::Role;

fn yolo_cfg() -> AppConfig {
    AppConfig {
      confirmations: "yolo".into(),
      api_base: "http://127.0.0.1:8080".into(),
      workdir: "/tmp".into(),
      transcripts_dir: "/tmp".into(),
      sweep_interval_ms: 250,
      model: "qwen3.8-27b".into(),
      max_context_tokens: 8192,
      tool_timeout_ms: 30000,
      abort_restart_server: false,
      llama_container: "test-llama".into(),
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
    assert!(restart_llama_server(path, "clanker-llama").is_ok(), "204 → ok");
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
    assert!(has_work(&pf2, &after, &cfg), "daemon continues after yolo");
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
    let cfg = AppConfig { confirmations: "ask".into(), ..yolo_cfg() };
    assert!(!questions_resolvable(&pf, &cfg));
    assert!(!has_work(&pf, &text, &cfg), "ask mode waits, no work");
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
    let cfg = AppConfig { confirmations: "ask".into(), ..yolo_cfg() };
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
    strip_abort_marker(&path, &src).unwrap();
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
    assert!(has_work(&pf, prose, &yolo_cfg()), "prose mention must not stall the file");
  }

  #[test]
  fn stream_writer_stops_and_strips_when_marker_appears_mid_stream() {
    let dir = temp_dir().join("slopfile-test-streamabort");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(
      &path,
      "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n",
    )
    .unwrap();

    let mut w = StreamWriter::new(
      &path,
      &read_to_string(&path).unwrap(),
      Some("<!-- slopfile:live {\"phase\":\"generating\"} -->".into()),
    );
    assert!(w.handle(&types::StreamEvent::Text("hello ".into())));
    {
      let mut f = read_to_string(&path).unwrap();
      f.push_str(transcript::ABORT_MARKER);
      f.push('\n');
      std::fs::write(&path, f).unwrap();
    }
    assert!(!w.handle(&types::StreamEvent::Text("world".into())));
    let after = read_to_string(&path).unwrap();
    assert!(after.contains("hello"), "text up to abort is kept");
    assert!(!after.contains("world"), "post-abort text dropped");
    assert!(!after.contains("slopfile:abort"), "marker stripped");
    assert!(
      after
        .trim_end()
        .ends_with("<!-- slopfile:live {\"phase\":\"generating\"} -->")
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn file_has_abort_drives_keep_going_unnverted() {
    let dir = temp_dir().join("slopfile-test-keepgoing-wiring");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, "### You\nhi\n").unwrap();

    assert!(!file_has_abort(&path), "marker-free file must keep going");
    let keep_going = || !file_has_abort(&path);
    assert!(keep_going(), "no marker -> keep_going() true (continue)");

    let mut f = read_to_string(&path).unwrap();
    f.push_str(transcript::ABORT_MARKER);
    f.push('\n');
    std::fs::write(&path, f).unwrap();

    assert!(file_has_abort(&path), "marker present -> has abort");
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
    let cfg = AppConfig { confirmations: "ask".into(), ..yolo_cfg() };
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
  fn truncate_keeps_system_and_drops_oldest_turns() {
    let mut msgs = vec![ChatMessage::system("sys prompt here")];
    for i in 1..=8 {
      msgs.push(ChatMessage::user(format!("user turn number {i} with some body text")));
      msgs.push(ChatMessage::assistant(format!("assistant answer number {i} with some body text")));
    }
    let counts: Vec<usize> = msgs.iter().map(|m| (msg_chars(m) / 4) + 1).collect();
    let small = truncate_messages(msgs.clone(), &counts, 12);
    assert_eq!(small[0].role, Role::System);
    assert!(small.len() < msgs.len(), "old turns dropped: {} -> {}",
      msgs.len(), small.len());
    let last = small.last().unwrap();
    assert_eq!(last.content.as_deref(), Some("assistant answer number 8 with some body text"));
    assert!(matches!(small[1].role, Role::User | Role::Assistant));

    let big = truncate_messages(msgs.clone(), &counts, 1_000_000);
    assert_eq!(big.len(), msgs.len());
  }

  #[test]
  fn truncate_cuts_tool_rounds_as_a_pair() {
    let mut msgs = vec![ChatMessage::system("sys")];
    for i in 1..=4 {
      msgs.push(ChatMessage::user(format!("u{i}")));
      let call = ToolCall::new(format!("id{i}"), "bash".to_string(), "{}".to_string());
      let mut asst = ChatMessage::assistant(format!("thinking then call {i}"));
      asst.tool_calls = Some(vec![call.clone()]);
      msgs.push(asst);
      msgs.push(ChatMessage::tool(call.id.clone(), "bash".to_string(), "out".to_string()));
    }
    let counts: Vec<usize> = msgs.iter().map(|m| (msg_chars(m) / 4) + 1).collect();
    let small = truncate_messages(msgs.clone(), &counts, 6);
    assert!(matches!(small[1].role, Role::User | Role::Assistant));
    let has_call = small.iter().any(|m| m.tool_calls.is_some());
    let has_result = small.iter().any(|m| m.role == Role::Tool);
    assert_eq!(has_call, has_result, "tool round is atomic");
  }

  #[test]
  fn draft_below_done_is_work() {
    let dir = temp_dir().join("slopfile-test-draft");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, format!("{}\n<!-- @done -->\nkeep running daemons\n", sample_with_question_block())).unwrap();
    let text = read_to_string(&path).unwrap();
    let pf = transcript::parse_file(&text);
    assert!(has_work(&pf, &text, &yolo_cfg()));
    assert_eq!(draft_tail_after_done(&text).map(|s| s.trim().to_string()), Some("keep running daemons".into()));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn context_full_note_is_daemon_owned_and_never_a_draft() {
    let base = "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n\n### Assistant\npartial reply here\n<!-- @done -->\n";
    let note = format!("{CONTEXT_FULL_NOTE}\n");

    let note_only = format!("{base}{note}");
    let text = &note_only;
    let pf = transcript::parse_file(text);
    assert!(draft_tail_after_done(text).is_none(), "note is not a draft");
    assert!(section_below_done(text).is_none(), "note is not a section");
    assert!(!has_work(&pf, text, &yolo_cfg()), "note-only tail must be idle");
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("context-full"))));

    let note_and_draft = format!("{base}{note}\ncontinue the work\n");
    let text = &note_and_draft;
    let pf = transcript::parse_file(text);
    assert_eq!(
      draft_tail_after_done(text).map(|s| s.trim().to_string()),
      Some("continue the work".into())
    );
    assert!(has_work(&pf, text, &yolo_cfg()));
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
    promote_draft(&path, &text).expect("promote");
    let after = read_to_string(&path).unwrap();
    let note_ix = after.find(CONTEXT_FULL_NOTE).expect("note kept");
    let you_ix = after.find("### You\nnext message").expect("section right before draft");
    let done_ix = after.find("<!-- @done -->").expect("done marker");
    assert!(done_ix < note_ix && note_ix < you_ix, "layout: {after}");
    let section = section_below_done(&after).expect("promoted section");
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
    assert!(tpl.contains("tools = [\"read\", \"write\", \"edit\", \"bash\", \"grep\", \"find\", \"ls\"]"));
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

    std::fs::remove_dir_all(&dir).ok();
  }

