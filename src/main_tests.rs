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
  fn yolo_resolves_questions_with_first_option() {
    let dir = std::env::temp_dir().join("slopfile-test-yolo");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, sample_with_question_block()).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    let pf = transcript::parse_file(&text);
    assert!(!pf.pending_questions.is_empty());
    let cfg = yolo_cfg();
    assert!(questions_resolvable(&pf, &cfg), "yolo is always resolvable");
    resolve_questions(&path, &text, &pf, &cfg).unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(!after.contains("❓ QUESTIONS"), "block removed");
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
    let dir = std::env::temp_dir().join("slopfile-test-ask");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, sample_with_question_block()).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    let pf = transcript::parse_file(&text);
    let cfg = AppConfig { confirmations: "ask".into(), ..yolo_cfg() };
    assert!(!questions_resolvable(&pf, &cfg));
    assert!(!has_work(&pf, &text, &cfg), "ask mode waits, no work");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn abort_marker_is_stripped_but_rest_preserved() {
    let dir = std::env::temp_dir().join("slopfile-test-abort");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    let src = format!(
      "{}\n\n<!-- slopfile:abort -->\n",
      sample_with_question_block()
    );
    std::fs::write(&path, src.clone()).unwrap();
    strip_abort_marker(&path, &src).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(!after.contains("slopfile:abort"));
    assert!(after.contains("### You"));
    assert!(after.contains("❓ QUESTIONS"), "user block untouched");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn stream_writer_stops_and_strips_when_marker_appears_mid_stream() {
    let dir = std::env::temp_dir().join("slopfile-test-streamabort");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(
      &path,
      "```toml\nmodel = \"qwen3.8-27b\"\n```\n# S\nSys.\n\n### You\nhi\n",
    )
    .unwrap();

    let mut w = StreamWriter::new(
      &path,
      &std::fs::read_to_string(&path).unwrap(),
      Some("<!-- slopfile:live {\"phase\":\"generating\"} -->".into()),
    );
    // Normal event: stream stuff in.
    assert!(w.handle(&types::StreamEvent::Text("hello ".into())));
    // Simulate the user appending an abort marker while streaming.
    {
      let mut f = std::fs::read_to_string(&path).unwrap();
      f.push_str(transcript::ABORT_MARKER);
      f.push('\n');
      std::fs::write(&path, f).unwrap();
    }
    // Next event: must stop (false) and the marker must not survive a flush.
    assert!(!w.handle(&types::StreamEvent::Text("world".into())));
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("hello"), "text up to abort is kept");
    assert!(!after.contains("world"), "post-abort text dropped");
    assert!(!after.contains("slopfile:abort"), "marker stripped");
    // Stale live status is rewritten by the caller (run_generation_turn
    // write_status idle); StreamWriter only guarantees it stays last.
    assert!(
      after
        .trim_end()
        .ends_with("<!-- slopfile:live {\"phase\":\"generating\"} -->")
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn draft_below_done_is_work() {
    let dir = std::env::temp_dir().join("slopfile-test-draft");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.md");
    std::fs::write(&path, format!("{}\n<!-- @done -->\nkeep running daemons\n", sample_with_question_block())).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let pf = transcript::parse_file(&text);
    assert!(has_work(&pf, &text, &yolo_cfg()));
    assert_eq!(draft_tail_after_done(&text).map(|s| s.trim().to_string()), Some("keep running daemons".into()));
    std::fs::remove_dir_all(&dir).ok();
  }
