use super::*;
use crate::types::Thinking;
use crate::questions::Question;

const FENCE: &str = "```toml\nmodel = \"qwen3.8-27b\"\nthinking = \"auto\"\n```\n";
const BEGIN_LINE: &str = "<!-- slopfile:begin -->\n";

fn region(name: &str, body: &str) -> String {
  format!("<!-- slopfile:{name} -->\n{}\n<!-- /slopfile -->\n", escape(body))
}

fn payload_region(name: &str, payload: &str) -> String {
  format!("<!-- slopfile:{name} {payload} -->\n<!-- /slopfile -->\n")
}

fn call_region(id: &str, name: &str, arguments: &str) -> String {
  payload_region(
    CALL,
    &json(&Call {
      id: id.to_string(),
      name: name.to_string(),
      arguments: arguments.to_string(),
    }),
  )
}

fn result_region(id: &str, name: &str, ok: bool, ms: u64, output: &str) -> String {
  format!(
    "<!-- slopfile:result {} -->\n{}\n<!-- /slopfile -->\n",
    json(&ResultHeader {
      id: id.to_string(),
      name: name.to_string(),
      ok,
      ms,
    }),
    escape(output)
  )
}

fn note_region(by: &str, text: &str) -> String {
  format!(
    "<!-- slopfile:note {} -->\n{}\n<!-- /slopfile -->\n",
    json(&serde_json::json!({"by": by})),
    escape(text)
  )
}

fn container(name: &str, children: &str) -> String {
  format!("<!-- slopfile:{name} -->\n{children}<!-- /slopfile -->\n")
}

fn session_file(body: &str) -> String {
  format!("{FENCE}{BEGIN_LINE}{body}<!-- slopfile:input -->\n")
}

fn input_file(text: &str) -> String {
  format!("{FENCE}{BEGIN_LINE}<!-- slopfile:input -->\n{text}")
}

fn unwrap_parts(escaped: &str) -> (String, Vec<String>) {
  let mut outside = String::new();
  let mut inner = Vec::new();
  let mut rest = escaped;
  while let Some(ix) = rest.find(ESC_OPEN) {
    outside.push_str(&rest[..ix]);
    let body = &rest[ix + ESC_OPEN.len()..];
    let end = body.find(ESC_CLOSE).expect("every wrapper must close");
    inner.push(body[..end].to_string());
    rest = &body[end + ESC_CLOSE.len()..];
  }
  outside.push_str(rest);
  (outside, inner)
}

fn sample_session() -> Session {
  let config = SessionConfig {
    model: Some("qwen3.8-27b".to_string()),
    thinking: Thinking::Auto,
    tool_timeout_ms: None,
    tools: Some(vec!["read".to_string(), "bash".to_string()]),
    tools_excluded: vec!["question".to_string()],
    max_context_tokens: Some(96_000),
  };
  let mut session = Session::new(config, "you are helpful\nsecond line");
  session.turns.push(Turn {
    user: Some("hi".to_string()),
    notes: vec![Note {
      by: NoteBy::Model,
      text: "still digging".to_string(),
    }],
    rounds: vec![Round {
      assistant: Assistant {
        reasoning: Some("look around first".to_string()),
        text: Some("on it".to_string()),
        calls: vec![Call {
          id: "c1".to_string(),
          name: "bash".to_string(),
          arguments: r#"{"command":"ls"}"#.to_string(),
        }],
      },
      results: vec![ToolOutcome {
        id: "c1".to_string(),
        name: "bash".to_string(),
        ok: true,
        ms: 9,
        output: "1: src".to_string(),
      }],
    }],
    question: None,
  });
  session.turns.push(Turn {
    user: Some("now compress that".to_string()),
    notes: vec![],
    rounds: vec![Round {
      assistant: Assistant {
        reasoning: None,
        text: Some("done".to_string()),
        calls: vec![],
      },
      results: vec![],
    }],
    question: None,
  });
  session
}

fn same_shape(a: &Session, b: &Session) -> bool {
  a.preamble.trim() == b.preamble.trim()
    && a.config == b.config
    && a.system == b.system
    && a.turns.len() == b.turns.len()
    && a.input == b.input
    && a.open == b.open
    && a
      .turns
      .iter()
      .zip(b.turns.iter())
      .all(|(x, y)| x.user == y.user && x.notes.len() == y.notes.len() && x.rounds.len() == y.rounds.len())
}

#[test]
fn escape_round_trips_every_token() {
  for token in ESC_TOKENS {
    let escaped = escape(token);
    assert_eq!(unescape(&escaped), *token, "token {token:?} must survive");
  }
  let body = ESC_TOKENS.join(" and ");
  assert_eq!(unescape(&escape(&body)), body);
}

#[test]
fn no_dangerous_token_survives_outside_a_wrapper() {
  let dangerous = [TOKEN_OPEN, TOKEN_CLOSE, ESC_OPEN, ESC_CLOSE];
  let bodies = [
    ESC_TOKENS.join(""),
    "<!---- slopfile:turn -->".to_string(),
    "<!- -!-[x]-->".to_string(),
    "<!--slopfile:ESC[<!-- /slopfile -->]-->".to_string(),
    "]--><!-- slopfile:[[".to_string(),
    "]-->".to_string(),
    "<!-- /slopfile -->".to_string(),
    "<!--slopfile:input -->".to_string(),
  ];
  for body in bodies.iter() {
    let escaped = escape(body);
    let (outside, inner) = unwrap_parts(&escaped);
    for token in dangerous {
      assert!(
        !outside.contains(token),
        "{token:?} escaped a wrapper in {body:?} -> {escaped:?}"
      );
    }
    assert!(
      inner.iter().all(|i| ESC_TOKENS.contains(&i.as_str())),
      "each wrapper holds exactly one known token: {inner:?}"
    );
  }
}

#[test]
fn escape_leaves_token_free_text_untouched() {
  let body = "plain text, a -- dash, angle brackets, 100% done, no tokens here";
  assert_eq!(escape(body), body);
  assert_eq!(unescape(body), body);
}

#[test]
fn escape_handles_nested_wrappers() {
  let body = format!("{} and {}", escape("<!-- /slopfile -->"), escape("<!--slopfile:ESC[x]-->"));
  assert_eq!(unescape(&escape(&body)), body);
  assert!(
    escape(&body).len() > body.len(),
    "nested wrappers must grow, not collapse: {body:?}"
  );
}

#[test]
fn escape_only_emits_wrappers_holding_a_single_token() {
  let bodies = [
    ESC_TOKENS.join(""),
    "<!---- slopfile:turn -->".to_string(),
    "<!- -!-[x]-->".to_string(),
    "<!--slopfile:ESC[<!-- /slopfile -->]-->".to_string(),
    "]--><!-- slopfile:[[".to_string(),
    "<!-- /slopfile -->".to_string(),
    "<".repeat(40),
    "no tokens at all".to_string(),
  ];
  for body in bodies.iter() {
    let escaped = escape(body);
    let mut rest = escaped.as_str();
    let mut wrappers = 0;
    while let Some(ix) = rest.find(ESC_OPEN) {
      wrappers += 1;
      rest = &rest[ix + ESC_OPEN.len()..];
      let end = rest.find(ESC_CLOSE).expect("every wrapper must close");
      let inner = &rest[..end];
      assert!(
        ESC_TOKENS.contains(&inner),
        "wrapper must hold one token, got {inner:?} in {escaped:?}"
      );
      rest = &rest[end + ESC_CLOSE.len()..];
    }
    assert_eq!(unescape(&escaped), *body, "round trip for {body:?}");
    assert!(wrappers <= body.len() + 1, "sanity bound");
  }
}

#[test]
fn decode_never_yields_structure() {
  let hostile = "<!-- slopfile:turn -->\n<!-- /slopfile -->\n<!--slopfile:ESC[lie]--> end";
  let file = session_file(&format!(
    "{}{}",
    region(SYSTEM, "sys"),
    container(TURN, &region(NOTE, hostile))
  ));
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(session.turns.len(), 1);
  assert_eq!(session.turns[0].notes[0].text, hostile);
  assert!(!session.turn_in_progress());
  assert_eq!(session.render(), file);
}

#[test]
fn render_parse_round_trip() {
  let session = sample_session();
  let rendered = session.render();
  let (parsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}");
  assert!(same_shape(&session, &parsed), "{rendered}");
  assert_eq!(parsed.turns[0].user.as_deref(), Some("hi"));
  assert_eq!(parsed.config.model.as_deref(), Some("qwen3.8-27b"));
  assert_eq!(parsed.config.tools_excluded, vec!["question".to_string()]);
  assert_eq!(parsed.system, "you are helpful\nsecond line");
  assert_eq!(parsed.turns[0].rounds[0].results[0].output, "1: src");
  assert_eq!(parsed.turns[0].notes[0].text, "still digging");
  assert_eq!(parsed.render(), rendered, "rendering is idempotent");
}

#[test]
fn render_marks_the_open_section_and_leaves_it_unclosed() {
  let mut session = sample_session();
  session.turns.push(Turn::default());
  session.begin_round();
  session.push_text("partial ans");
  let rendered = session.render();
  assert!(
    rendered.contains("<!-- slopfile:text -->\npartial ans\n<!-- slopfile:input -->"),
    "{rendered}"
  );

  let (parsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(parsed.section(), Some(TEXT));
  assert!(parsed.turn_in_progress());
  assert_eq!(
    parsed.open.as_deref(),
    Some(["turn".to_string(), "assistant".to_string(), "text".to_string()].as_slice())
  );
  assert_eq!(
    parsed.turns[2].rounds[0].assistant.text.as_deref(),
    Some("partial ans")
  );
  assert_eq!(parsed.render(), rendered);
}

#[test]
fn close_section_ends_the_text_region() {
  let mut session = Session::default();
  session.turns.push(Turn::default());
  session.begin_round();
  session.push_reasoning("think ");
  session.push_reasoning("hard");
  session.push_text("answer");
  assert_eq!(session.section(), Some(TEXT));
  session.close_section();
  assert_eq!(session.section(), None);
  let rendered = session.render();
  let (parsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(
    parsed.turns[0].rounds[0].assistant.reasoning.as_deref(),
    Some("think hard")
  );
  assert_eq!(
    parsed.turns[0].rounds[0].assistant.text.as_deref(),
    Some("answer")
  );
}

#[test]
fn turn_complete_iff_closed() {
  let closed = session_file(&container(TURN, ""));
  let (session, report) = parse(&closed);
  assert!(report.is_clean(), "{report:?}");
  assert!(!session.turn_in_progress());
  assert!(session.last_turn_complete());
  assert_eq!(session.open, None);

  let open = format!("{FENCE}{BEGIN_LINE}<!-- slopfile:turn -->\n<!-- slopfile:assistant -->\nthinking\n");
  let (session, _) = parse(&open);
  assert!(session.turn_in_progress());
  assert!(!session.last_turn_complete());
  assert_eq!(
    session.open.as_deref(),
    Some(["turn".to_string(), "assistant".to_string()].as_slice())
  );
}

#[test]
fn parse_open_region_owns_rest_of_file() {
  let file = format!(
    "{FENCE}{BEGIN_LINE}<!-- slopfile:turn -->\n<!-- slopfile:assistant -->\n<!-- slopfile:text -->\nhalf a sen"
  );
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert!(report.swallowed.is_empty(), "{:?}", report.swallowed);
  assert_eq!(
    session.open.as_deref(),
    Some(["turn".to_string(), "assistant".to_string(), "text".to_string()].as_slice())
  );
  assert_eq!(
    session.turns[0].rounds[0].assistant.text.as_deref(),
    Some("half a sen")
  );
}

#[test]
fn unclosed_leaf_followed_by_a_region_is_reported() {
  let file = format!(
    "{FENCE}{BEGIN_LINE}<!-- slopfile:turn -->\n<!-- slopfile:assistant -->\n<!-- slopfile:text -->\nleaky\n<!-- slopfile:user -->\nnope\n<!-- /slopfile -->\n"
  );
  let (session, report) = parse(&file);
  assert!(!report.is_clean());
  assert_eq!(report.swallowed.len(), 1);
  let problems = report.problems();
  assert!(problems[0].contains("left unclosed"), "{problems:?}");
  assert!(problems[0].contains("user"), "the report must name what it ate: {problems:?}");
  assert_eq!(
    session.turns[0].rounds[0].assistant.text.as_deref(),
    Some("leaky\n<!-- slopfile:user -->\nnope\n<!-- /slopfile -->\n")
  );
}

#[test]
fn duplicate_close_is_reported() {
  let file = format!("{FENCE}{BEGIN_LINE}<!-- /slopfile -->\n");
  let (_, report) = parse(&file);
  assert!(!report.is_clean());
  assert!(report.duplicate_close);
  assert!(report.problems()[0].contains("never opened"));
}

#[test]
fn unknown_region_is_reported() {
  let file = session_file(&region("thinkin", "hmm"));
  let (session, report) = parse(&file);
  assert_eq!(report.unknown_regions, vec!["thinkin".to_string()]);
  assert!(report.problems()[0].contains("unknown region `thinkin`"));
  assert_eq!(session.turns.len(), 0, "unknown regions are not turns");
  assert!(!session.dangling(), "the region was closed, so nothing dangles");
}

#[test]
fn input_is_eof_terminated_and_opaque() {
  let hostile = "<!-- slopfile:turn -->\n<!-- /slopfile -->\n<!--slopfile:ESC[x]-->";
  let file = format!("{FENCE}{BEGIN_LINE}<!-- slopfile:input -->\n{hostile}");
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(session.input, hostile);
  assert_eq!(session.turns.len(), 0, "typed markers never become structure");
  assert_eq!(session.render(), file);
}

#[test]
fn empty_input_round_trips() {
  let file = session_file("");
  let (session, _) = parse(&file);
  assert_eq!(session.input, "");
  assert_eq!(session.render(), file);
}

#[test]
fn request_messages_pairs_tool_calls_with_results() {
  let assistant = container(
    ASSISTANT,
    &format!(
      "{}{}{}{}",
      region(REASONING, "thinking"),
      region(TEXT, "on it"),
      call_region("c1", "bash", r#"{"command":"ls"}"#),
      call_region("c2", "read", r#"{"path":"a"}"#)
    ),
  );
  let turn = container(
    TURN,
    &format!(
      "{}{}{}{}",
      region(USER, "hi"),
      assistant,
      result_region("c1", "bash", true, 4, "1: src"),
      result_region("c2", "read", true, 1, "1: hi")
    ),
  );
  let (session, report) = parse(&session_file(&format!("{}{}", region(SYSTEM, "sys"), turn)));
  assert!(report.is_clean(), "{report:?}");

  let messages = session.request_messages();
  assert_eq!(messages.len(), 5);
  assert_eq!(messages[0].role, Role::System);
  assert_eq!(messages[1].role, Role::User);
  assert_eq!(messages[1].content.as_deref(), Some("hi"));
  assert_eq!(messages[2].role, Role::Assistant);
  assert_eq!(messages[2].reasoning_content.as_deref(), Some("thinking"));
  assert_eq!(messages[2].content.as_deref(), Some("on it"));
  assert_eq!(messages[2].tool_calls.as_ref().unwrap().len(), 2);
  assert_eq!(messages[3].role, Role::Tool);
  assert_eq!(messages[3].tool_call_id.as_deref(), Some("c1"));
  assert_eq!(messages[3].content.as_deref(), Some("1: src"));
  assert_eq!(messages[4].tool_call_id.as_deref(), Some("c2"));
  assert_eq!(messages[4].content.as_deref(), Some("1: hi"));
}

#[test]
fn request_messages_synthesizes_a_call_for_an_orphan_result() {
  let mut session = Session::default();
  session.turns.push(Turn {
    user: Some("go".to_string()),
    notes: vec![],
    rounds: vec![Round {
      assistant: Assistant::default(),
      results: vec![ToolOutcome {
        id: "orphan".to_string(),
        name: "bash".to_string(),
        ok: true,
        ms: 0,
        output: "out".to_string(),
      }],
    }],
    question: None,
  });
  let messages = session.request_messages();
  assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "orphan");
  assert_eq!(messages[2].role, Role::Tool);
  assert_eq!(messages[2].tool_call_id.as_deref(), Some("orphan"));
}

#[test]
fn multi_round_turn_keeps_each_assistant_with_its_own_results() {
  let round = |id: &str, text: &str, out: &str| Round {
    assistant: Assistant {
      reasoning: None,
      text: Some(text.to_string()),
      calls: vec![Call {
        id: id.to_string(),
        name: "bash".to_string(),
        arguments: "{}".to_string(),
      }],
    },
    results: vec![ToolOutcome {
      id: id.to_string(),
      name: "bash".to_string(),
      ok: true,
      ms: 0,
      output: out.to_string(),
    }],
  };
  let mut session = Session::default();
  session.turns.push(Turn {
    user: Some("go".to_string()),
    notes: vec![],
    rounds: vec![round("a", "first", "one"), round("b", "second", "two")],
    question: None,
  });
  let rendered = session.render();
  let (parsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(parsed.turns[0].rounds.len(), 2);
  assert_eq!(parsed.turns[0].rounds[0].results[0].output, "one");
  assert_eq!(parsed.turns[0].rounds[1].results[0].output, "two");
  let messages = parsed.request_messages();
  assert_eq!(messages.len(), 5, "no system prompt in this fixture");
  assert_eq!(messages[0].role, Role::User);
  assert_eq!(messages[1].content.as_deref(), Some("first"));
  assert_eq!(messages[2].content.as_deref(), Some("one"));
  assert_eq!(messages[3].content.as_deref(), Some("second"));
  assert_eq!(messages[4].content.as_deref(), Some("two"));
}

#[test]
fn notes_carry_their_authorship() {
  let file = session_file(&container(
    TURN,
    &format!(
      "{}{}",
      note_region("model", "model scratchpad"),
      note_region("harness", "prune now")
    ),
  ));
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  let notes = &session.turns[0].notes;
  assert_eq!(notes.len(), 2);
  assert_eq!(notes[0].by, NoteBy::Model);
  assert_eq!(notes[1].by, NoteBy::Harness);
  let messages = session.request_messages();
  assert_eq!(messages[0].role, Role::Assistant);
  assert_eq!(messages[0].content.as_deref(), Some("model scratchpad"));
  assert_eq!(messages[1].role, Role::User);
  assert_eq!(messages[1].content.as_deref(), Some("prune now"));
}

#[test]
fn question_round_trip_and_answer() {
  let questions = vec![Question {
    id: 1,
    question: "red or blue".to_string(),
    answers: vec!["red".to_string(), "blue".to_string()],
    default: None,
  }];
  let pending = PendingQuestion {
    call: Call {
      id: "q1".to_string(),
      name: "question".to_string(),
      arguments: r#"{"data":[]}"#.to_string(),
    },
    questions: questions.clone(),
  };
  let mut session = Session::default();
  session.turns.push(Turn {
    user: Some("pick".to_string()),
    notes: vec![],
    rounds: vec![Round {
      assistant: Assistant {
        reasoning: None,
        text: Some("which?".to_string()),
        calls: vec![],
      },
      results: vec![],
    }],
    question: None,
  });
  session.set_question(pending);

  let rendered = session.render();
  let (mut parsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(parsed.pending_question().expect("pending").questions, questions);
  let messages = parsed.request_messages();
  assert_eq!(messages.len(), 3);
  assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "q1");
  assert_eq!(messages[2].role, Role::Tool);
  assert_eq!(messages[2].content.as_deref(), Some(AWAITING_ANSWER));

  parsed.answer_question(vec![(1, "blue".to_string())]).expect("answer");
  assert!(parsed.pending_question().is_none());
  let messages = parsed.request_messages();
  assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "q1");
  assert!(
    messages[2].content.as_deref().unwrap().contains("\"answer\":\"blue\""),
    "{:?}",
    messages[2].content
  );
  let (mut reread, report) = parse(&parsed.render());
  assert!(report.is_clean(), "{report:?}");
  assert!(reread.pending_question().is_none());
  assert!(reread.answer_question(vec![]).is_err(), "nothing left to answer");
}

#[test]
fn answer_question_clears_the_answer_line() {
  let mut session = sample_session();
  session.input = "1\n".to_string();
  session.set_question(PendingQuestion {
    call: Call {
      id: "q1".to_string(),
      name: "question".to_string(),
      arguments: "{}".to_string(),
    },
    questions: vec![Question {
      id: 1,
      question: "ok?".to_string(),
      answers: vec!["yes".to_string(), "no".to_string()],
      default: None,
    }],
  });
  session.answer_question(vec![(1, "yes".to_string())]).expect("answer");
  assert_eq!(session.input, "");
  let (parsed, report) = parse(&session.render());
  assert!(report.is_clean(), "{report:?}");
  assert!(!parsed.has_input());
  assert!(parsed.pending_question().is_none());
}

#[test]
fn rewinding_an_open_round_hands_back_the_partial_output() {
  let mut session = sample_session();
  session.begin_round();
  session.push_reasoning("half thought");
  session.push_text("half answer");
  let (mut parsed, _) = parse(&session.render());
  assert_eq!(parsed.section(), Some(TEXT));

  let (reasoning, text) = parsed.rewind_open_round();
  assert_eq!(reasoning.as_deref(), Some("half thought"));
  assert_eq!(text.as_deref(), Some("half answer"));
  assert_eq!(parsed.section(), None, "the assistant region is open again");
  assert!(parsed.turn_in_progress());
  let base = parsed.render();
  assert!(!base.contains("half thought"), "{base}");
  assert!(
    base.contains("<!-- slopfile:assistant -->\n<!-- slopfile:input -->\n"),
    "{base}"
  );

  parsed.push_reasoning(reasoning.as_deref().unwrap_or_default());
  parsed.push_text(text.as_deref().unwrap_or_default());
  parsed.push_text(" and the rest");
  let (again, report) = parse(&parsed.render());
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(
    again.turns.last().unwrap().rounds.last().unwrap().assistant.text.as_deref(),
    Some("half answer and the rest")
  );
}

#[test]
fn promote_input_turns_typed_text_into_a_user_turn() {
  let (mut session, report) = parse(&input_file("do the thing"));
  assert!(report.is_clean(), "{report:?}");
  assert!(session.has_input());
  assert!(session.promote_input());
  assert!(!session.has_input());
  assert_eq!(session.turns[0].user.as_deref(), Some("do the thing"));
  let messages = session.request_messages();
  assert_eq!(messages.last().unwrap().role, Role::User);
  assert_eq!(
    messages.last().unwrap().content.as_deref(),
    Some("do the thing")
  );
  assert!(!session.promote_input(), "nothing left to promote");
}

#[test]
fn promote_input_escapes_typed_markers() {
  let (mut session, report) = parse(&input_file("<!-- /slopfile --> please"));
  assert!(report.is_clean(), "{report:?}");
  session.promote_input();
  let rendered = session.render();
  assert!(
    rendered.contains("<!--slopfile:ESC[<<!--/slopfile:ESC-->!-- /slopfile --> please"),
    "the typed marker's `<` is wrapped: {rendered}"
  );
  let (reparsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(
    reparsed.turns[0].user.as_deref(),
    Some("<!-- /slopfile --> please")
  );
  assert_eq!(reparsed.turns[0].rounds.len(), 0);
}

#[test]
fn preamble_is_preserved_verbatim() {
  let preamble = "# my notes\n\n```toml\nmodel = \"custom-model\"\n```\n";
  let file = format!("{preamble}{BEGIN_LINE}<!-- slopfile:input -->\n");
  let (session, _) = parse(&file);
  assert_eq!(session.config.model.as_deref(), Some("custom-model"));
  let rendered = session.render();
  assert!(rendered.starts_with("# my notes\n\n```toml"), "{rendered}");
  assert!(
    rendered.contains("model = \"custom-model\"") && rendered.contains("thinking = \"auto\""),
    "the fence is regenerated from the config: {rendered}"
  );
  assert_eq!(parse(&rendered).0.render(), rendered, "rendering is stable");
}

#[test]
fn preamble_without_a_toml_fence_yields_default_config() {
  let file = format!("no fence here\n{BEGIN_LINE}<!-- slopfile:input -->\n");
  let (session, _) = parse(&file);
  assert_eq!(session.config, SessionConfig::default());
  assert_eq!(session.render(), file);
}

#[test]
fn crlf_files_parse_and_render_as_lf() {
  let file = input_file("hello\n").replace('\n', "\r\n");
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(session.input, "hello\n", "crlf input is normalized");
  assert!(!session.preamble.contains('\r'));
  let rendered = session.render();
  assert!(!rendered.contains("\r"), "structure is lf-only: {rendered:?}");
  assert!(rendered.ends_with("hello\n"), "{rendered:?}");
  assert_eq!(parse(&rendered).0.input, "hello\n", "stable");
}

#[test]
fn toml_fence_round_trips_through_the_parser() {
  let config = SessionConfig {
    model: Some("m".to_string()),
    thinking: Thinking::Auto,
    tool_timeout_ms: Some(1234),
    tools: Some(vec!["read".to_string()]),
    tools_excluded: vec!["subagent".to_string()],
    max_context_tokens: Some(4096),
  };
  let session = Session::new(config, "sys");
  let (parsed, _) = parse(&session.render());
  assert_eq!(parsed.config, session.config);
}

#[test]
fn finish_round_records_calls_and_results() {
  let mut session = Session::default();
  session.turns.push(Turn::default());
  session.begin_round();
  session.push_text("calling");
  session.finish_round(
    vec![Call {
      id: "c9".to_string(),
      name: "bash".to_string(),
      arguments: "{}".to_string(),
    }],
    vec![ToolOutcome {
      id: "c9".to_string(),
      name: "bash".to_string(),
      ok: false,
      ms: 12,
      output: "boom".to_string(),
    }],
  );
  let rendered = session.render();
  let (parsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(parsed.section(), None);
  assert!(parsed.open.is_none(), "the turn closed");
  let round = &parsed.turns[0].rounds[0];
  assert_eq!(round.assistant.text.as_deref(), Some("calling"));
  assert_eq!(round.assistant.calls[0].id, "c9");
  assert_eq!(round.results[0].output, "boom");
  assert!(!round.results[0].ok);
  assert_eq!(parsed.render(), rendered);
}

#[test]
fn closing_a_turn_leaves_the_file_complete() {
  let mut session = sample_session();
  session.begin_round();
  session.push_text("partial");
  assert!(session.turn_in_progress());
  session.close_turn();
  assert!(!session.turn_in_progress());
  assert!(session.last_turn_complete());
  let (parsed, report) = parse(&session.render());
  assert!(report.is_clean(), "{report:?}");
  assert!(parsed.last_turn_complete());
}

#[test]
fn system_region_is_optional_and_replaced_not_appended() {
  let file = session_file(&region(SYSTEM, "one"));
  let (session, _) = parse(&file);
  assert_eq!(session.system, "one");
  assert_eq!(session.render(), file);
}

#[test]
fn a_malformed_payload_is_reported_not_silently_invented() {
  let file = session_file(&container(
    TURN,
    &format!(
      "{}\n{}",
      container(ASSISTANT, &payload_region(CALL, "{not json")),
      "<!-- slopfile:result {nope -->\nout\n<!-- /slopfile -->\n"
    ),
  ));
  let (session, report) = parse(&file);
  assert!(!report.is_clean(), "a corrupt header must not pass as clean");
  let problems = report.problems().join("; ");
  assert!(problems.contains("call"), "{problems}");
  assert!(problems.contains("result"), "{problems}");
  assert!(problems.contains("turn"), "the report must locate the region: {problems}");
  let round = &session.turns[0].rounds[0];
  assert!(round.assistant.calls.is_empty(), "an unparseable call is not invented");
  assert!(
    !round.results[0].ok,
    "an unreadable result is never reported as a success"
  );
  assert!(round.results[0].output.contains("cannot be trusted"), "{:?}", round.results[0]);
}

#[test]
fn a_malformed_question_payload_is_reported() {
  let file = session_file(&container(TURN, &payload_region(QUESTION, "{oops")));
  let (session, report) = parse(&file);
  assert!(!report.is_clean());
  assert!(report.problems()[0].contains("question"), "{:?}", report.problems());
  assert!(session.pending_question().is_none());
}

#[test]
fn bytes_outside_ascii_never_split_a_character() {
  let body = "héllo → wörld ⟶ done";
  assert_eq!(unescape(&escape(body)), body);
  assert_eq!(escape(body), body);
}

#[test]
fn a_tool_output_full_of_brackets_still_round_trips() {
  let output = "drwxr-xr-x 2 root root 4096 Jan  1 00:00 [drwxr-xr-x] {a,b}\n[[[nested]]]";
  let file = session_file(&container(TURN, &result_region("c1", "bash", true, 3, output)));
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(session.turns[0].rounds[0].results[0].output, output);
}

// ---- round-1 review regressions

#[test]
fn pending_input_survives_an_open_turn() {
  // The window between opening a round and the first streamed token is real: every flush writes
  // the file with the turn still open, so the input must not be swallowed by it.
  let mut cases: Vec<(Option<Vec<String>>, Session)> = Vec::new();
  {
    let mut s = Session::default();
    s.turns.push(Turn { user: Some("hi".into()), ..Turn::default() });
    s.open = Some(vec!["turn".into()]);
    cases.push((s.open.clone(), s));
  }
  {
    let mut s = Session::default();
    s.turns.push(Turn { user: Some("hi".into()), ..Turn::default() });
    s.begin_round();
    s.open = Some(vec!["turn".into(), "assistant".into()]);
    cases.push((s.open.clone(), s));
  }
  {
    let mut s = Session::default();
    s.turns.push(Turn { user: Some("hi".into()), ..Turn::default() });
    s.begin_round();
    s.push_text("thinking out loud");
    s.open = Some(vec!["turn".into(), "assistant".into(), "text".into()]);
    cases.push((s.open.clone(), s));
  }
  for (open, mut session) in cases {
    session.input = "AND ALSO FIX THE CONFIG".to_string();
    let rendered = session.render();
    let (parsed, report) = parse(&rendered);
    assert!(report.is_clean(), "{open:?}: {report:?}");
    assert_eq!(
      parsed.input, "AND ALSO FIX THE CONFIG",
      "input lost with open={open:?}"
    );
    assert_eq!(parsed.open, open, "the open region must be recovered");
    assert_eq!(parsed.render(), rendered, "render must be a fixed point");
  }
}

#[test]
fn a_file_with_no_begin_marker_keeps_its_prose() {
  let text = "# my important task notes\nplease refactor the parser\n";
  let (session, report) = parse(text);
  assert!(report.is_clean(), "{report:?}");
  assert!(
    session.render().starts_with(text),
    "hand-written notes must survive: {}",
    session.render()
  );
  assert_eq!(session.turns.len(), 0);
}

#[test]
fn a_marker_line_inside_the_fence_is_not_structure() {
  let fence = "```toml\nmodel = \"special-model\"\nhelp = \"\"\"\n<!-- slopfile:begin -->\n\"\"\"\n```\n";
  let text = format!(
    "{fence}<!-- slopfile:begin -->\n{}\n<!-- slopfile:input -->\n",
    region(SYSTEM, "sys")
  );
  let (session, report) = parse(&text);
  assert!(
    report.is_clean(),
    "a marker inside the fence is config, not a break: {report:?}"
  );
  assert_eq!(
    session.config.model.as_deref(),
    Some("special-model"),
    "the fence must be read as TOML, not truncated at the fake marker"
  );
  assert_eq!(session.system, "sys");
}

#[test]
fn an_unreadable_fence_is_preserved_verbatim() {
  let fence = "```toml\nmodel = \"special-model\"\nthis is not = = toml\n```\n";
  let text = format!("{fence}<!-- slopfile:begin -->\n<!-- slopfile:input -->\n");
  let (session, _) = parse(&text);
  assert!(
    session.render().starts_with(fence),
    "the human's fence text must survive: {}",
    session.render()
  );
  assert!(session.render().contains("this is not = = toml"));
}

#[test]
fn answering_a_question_in_a_round_less_turn_does_not_panic() {
  let mut session = Session::default();
  session.turns.push(Turn {
    user: Some("pick".to_string()),
    notes: vec![],
    rounds: vec![],
    question: Some(PendingQuestion {
      call: Call {
        id: "q1".into(),
        name: "question".into(),
        arguments: "{}".into(),
      },
      questions: vec![Question {
        id: 1,
        question: "ok?".into(),
        answers: vec!["yes".into(), "no".into()],
        default: None,
      }],
    }),
  });
  session
    .answer_question(vec![(1, "yes".to_string())])
    .expect("a question without an assistant region is still answerable");
  let messages = session.request_messages();
  assert_eq!(messages.len(), 3, "{messages:?}");
  assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "q1");
  assert!(messages[2].content.as_deref().unwrap().contains("yes"));
  assert!(session.answer_question(vec![]).is_err(), "nothing left to answer");
}

#[test]
fn promoting_input_never_overwrites_an_earlier_user_message() {
  // An abort before the first token leaves a turn holding a user message and an empty round.
  let mut session = Session::default();
  session.turns.push(Turn {
    user: Some("first request".to_string()),
    notes: vec![],
    rounds: vec![Round::default()],
    question: None,
  });
  session.input = "and also fix the lint".to_string();
  session.promote_input();
  assert_eq!(
    session.turns[0].user.as_deref(),
    Some("first request"),
    "the earlier message must survive"
  );
  assert_eq!(session.turns.len(), 2);
  assert_eq!(session.turns[1].user.as_deref(), Some("and also fix the lint"));
}

#[test]
fn a_pruned_empty_turn_absorbs_the_next_message() {
  let mut session = Session::default();
  session.turns.push(Turn::default());
  session.input = "first".to_string();
  session.promote_input();
  assert_eq!(session.turns.len(), 1, "an empty turn is the pending one");
  assert_eq!(session.turns[0].user.as_deref(), Some("first"));
}

#[test]
fn every_call_gets_exactly_one_result() {
  let calls = || {
    vec![
      Call { id: "".into(), name: "bash".into(), arguments: "{}".into() },
      Call { id: "".into(), name: "bash".into(), arguments: "{}".into() },
    ]
  };
  let mut session = Session::default();
  session.turns.push(Turn {
    user: Some("go".into()),
    ..Turn::default()
  });
  session.begin_round();
  session.finish_round(
    calls(),
    vec![
      ToolOutcome { id: "".into(), name: "bash".into(), ok: true, ms: 1, output: "one".into() },
      ToolOutcome { id: "".into(), name: "bash".into(), ok: true, ms: 1, output: "two".into() },
    ],
  );
  let messages = session.request_messages();
  let sent: Vec<String> = messages
    .iter()
    .filter_map(|m| m.tool_calls.as_ref())
    .flatten()
    .map(|c| c.id.clone())
    .collect();
  let answered: Vec<&str> = messages
    .iter()
    .filter_map(|m| m.tool_call_id.as_deref())
    .collect();
  assert_eq!(sent.len(), 2);
  assert_eq!(answered.len(), 2, "one result per call");
  let mut unique = sent.clone();
  unique.sort();
  unique.dedup();
  assert_eq!(unique.len(), 2, "ids must be unique: {sent:?}");
  assert_eq!(answered[0], sent[0]);
  assert_eq!(answered[1], sent[1]);
}

#[test]
fn a_call_the_harness_skips_still_gets_a_result() {
  // `solve` stops the round early, so later calls have no outcome of their own.
  let mut session = Session::default();
  session.turns.push(Turn {
    user: Some("go".into()),
    ..Turn::default()
  });
  session.begin_round();
  session.finish_round(
    vec![
      Call { id: "c1".into(), name: "solve".into(), arguments: "{}".into() },
      Call { id: "c2".into(), name: "bash".into(), arguments: "{}".into() },
    ],
    vec![ToolOutcome {
      id: "c1".into(),
      name: "solve".into(),
      ok: true,
      ms: 1,
      output: "solved".into(),
    }],
  );
  let messages = session.request_messages();
  let answered: Vec<&str> = messages
    .iter()
    .filter_map(|m| m.tool_call_id.as_deref())
    .collect();
  assert_eq!(answered, vec!["c1", "c2"], "both calls are answered");
  assert!(
    messages[3].content.as_deref().unwrap().contains("not executed"),
    "the skipped call says so: {:?}",
    messages[3].content
  );
  let (_, report) = parse(&session.render());
  assert!(report.is_clean(), "{report:?}");
}

#[test]
fn regions_out_of_place_are_reported_not_dropped() {
  let note_at_top = session_file(&region(NOTE, "IMPORTANT SCRATCH"));
  let (_, report) = parse(&note_at_top);
  assert!(!report.is_clean(), "a note at the top level is lost otherwise");
  assert!(report.problems()[0].contains("out of place"), "{:?}", report.problems());

  let two_texts = session_file(&container(
    TURN,
    &container(
      ASSISTANT,
      &format!("{}{}", region(TEXT, "first version"), region(TEXT, "second")),
    ),
  ));
  let (session, report) = parse(&two_texts);
  assert!(!report.is_clean(), "the first text would be silently replaced");
  assert!(report.problems()[0].contains("second `text`"), "{:?}", report.problems());
  assert_eq!(
    session.turns[0].rounds[0].assistant.text.as_deref(),
    Some("first version"),
    "the first body is kept until the file is fixed"
  );

  let nested = session_file(&container(
    TURN,
    &container(TURN, &region(USER, "inner")),
  ));
  let (_, report) = parse(&nested);
  assert!(!report.is_clean(), "a turn inside a turn is lost otherwise");
  assert!(report.problems()[0].contains("out of place"), "{:?}", report.problems());

  let two_users = session_file(&container(
    TURN,
    &format!("{}{}", region(USER, "one"), region(USER, "two")),
  ));
  let (_, report) = parse(&two_users);
  assert!(!report.is_clean(), "a second user region would be overwritten");
  assert!(report.problems()[0].contains("second `user`"), "{:?}", report.problems());
}

#[test]
fn a_model_note_never_produces_two_adjacent_assistant_messages() {
  let mut session = Session::default();
  session.turns.push(Turn {
    user: None,
    notes: vec![Note {
      by: NoteBy::Model,
      text: "remember: the user wants tests".to_string(),
    }],
    rounds: vec![Round {
      assistant: Assistant {
        reasoning: None,
        text: Some("Understood.".to_string()),
        calls: vec![],
      },
      results: vec![],
    }],
    question: None,
  });
  let messages = session.request_messages();
  assert_eq!(messages.len(), 1, "one message, not two: {messages:?}");
  let content = messages[0].content.as_deref().unwrap();
  assert!(content.contains("remember"), "{content}");
  assert!(content.contains("Understood"), "{content}");
}

#[test]
fn rewinding_a_round_drops_its_calls_too() {
  let mut session = Session::default();
  session.turns.push(Turn {
    user: Some("go".into()),
    ..Turn::default()
  });
  session.begin_round();
  session.push_text("half");
  session.finish_round(
    vec![Call { id: "c1".into(), name: "bash".into(), arguments: "{}".into() }],
    vec![ToolOutcome {
      id: "c1".into(),
      name: "bash".into(),
      ok: true,
      ms: 0,
      output: "out".into(),
    }],
  );
  let (reasoning, text) = session.rewind_open_round();
  assert_eq!(text.as_deref(), Some("half"));
  assert_eq!(reasoning, None);
  assert!(session.turns[0].rounds[0].assistant.calls.is_empty());
  assert!(session.turns[0].rounds[0].results.is_empty());
  for message in session.request_messages() {
    assert!(
      message.tool_calls.is_none(),
      "no orphan call survives the rewind: {message:?}"
    );
  }
}
