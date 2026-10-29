use super::*;
use crate::questions::Question;
use crate::types::Thinking;
use std::collections::BTreeMap;

const FENCE: &str = "```toml\nmodel = \"qwen3.8-27b\"\nthinking = \"auto\"\n```\n";
const BEGIN_LINE: &str = "<!-- slopfile:begin -->\n";

fn region(name: &str, body: &str) -> String {
  format!(
    "<!-- slopfile:{name} -->\n{}\n<!-- /slopfile -->\n",
    escape(body)
  )
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
    provider: Some("openrouter".to_string()),
    headers: BTreeMap::new(),
    burst_max_turns: None,
    tell_model_context_size: None,
    model: Some("qwen3.8-27b".to_string()),
    thinking: Thinking::Auto,
    tool_timeout_ms: None,
    tools: Some(vec!["read".to_string(), "bash".to_string()]),
    tools_excluded: vec!["question".to_string()],
    max_context_tokens: Some(96_000),
  };
  let mut session = Session::new(config, "you are helpful\nsecond line");
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("hi".to_string()),
    notes: vec![],
    rounds: vec![Round {
      notes: vec![Note {
        by: NoteBy::Model,
        text: "still digging".to_string(),
      }],
      assistant: Assistant {
        custom: Vec::new(),
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
    custom: Vec::new(),
    user: Some("now compress that".to_string()),
    notes: vec![],
    rounds: vec![Round {
      notes: Vec::new(),
      assistant: Assistant {
        custom: Vec::new(),
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
    && a.turns.iter().zip(b.turns.iter()).all(|(x, y)| {
      x.user == y.user
        && x.notes.len() == y.notes.len()
        && x.rounds.len() == y.rounds.len()
        && x
          .rounds
          .iter()
          .zip(y.rounds.iter())
          .all(|(p, q)| p.notes.len() == q.notes.len())
    })
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
  let body = format!(
    "{} and {}",
    escape("<!-- /slopfile -->"),
    escape("<!--slopfile:ESC[x]-->")
  );
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
  assert_eq!(parsed.turns[0].rounds[0].notes[0].text, "still digging");
  assert_eq!(parsed.render(), rendered, "rendering is idempotent");
}

#[test]
fn custom_assistant_regions_render_closed_and_round_trip() {
  let file = session_file(&container(
    "turn",
    &format!(
      "{}{}{}",
      region("user", "hi"),
      container(
        "assistant",
        &format!(
          "{}{}",
          region("reasoning", "think"),
          region("scratchpad", "trying two approaches")
        )
      ),
      result_region("c1", "bash", true, 1, "ok")
    ),
  ));
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(
    session.turns[0].rounds[0].assistant.custom,
    vec![(
      "scratchpad".to_string(),
      "trying two approaches".to_string()
    )]
  );

  let rendered = session.render();
  let (parsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}\n{rendered}");
  assert_eq!(parsed.turns[0].rounds[0].results.len(), 1);
  assert_eq!(
    parsed.turns[0].rounds[0].assistant.custom,
    session.turns[0].rounds[0].assistant.custom
  );
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
    Some(
      [
        "turn".to_string(),
        "assistant".to_string(),
        "text".to_string()
      ]
      .as_slice()
    )
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
  assert_eq!(session.open, None);

  let open =
    format!("{FENCE}{BEGIN_LINE}<!-- slopfile:turn -->\n<!-- slopfile:assistant -->\nthinking\n");
  let (session, _) = parse(&open);
  assert!(session.turn_in_progress());
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
    Some(
      [
        "turn".to_string(),
        "assistant".to_string(),
        "text".to_string()
      ]
      .as_slice()
    )
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
  assert!(
    problems[0].contains("user"),
    "the report must name what it ate: {problems:?}"
  );
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
  assert!(
    session.open.is_none(),
    "the region was closed, so nothing dangles"
  );
}

#[test]
fn input_is_eof_terminated_and_opaque() {
  let hostile = "<!-- slopfile:turn -->\n<!-- /slopfile -->\n<!--slopfile:ESC[x]-->";
  let file = format!("{FENCE}{BEGIN_LINE}<!-- slopfile:input -->\n{hostile}");
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(session.input, hostile);
  assert_eq!(
    session.turns.len(),
    0,
    "typed markers never become structure"
  );
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
    custom: Vec::new(),
    user: Some("go".to_string()),
    notes: vec![],
    rounds: vec![Round {
      notes: Vec::new(),
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
  // ids are renumbered to short per-round ones on the way out
  assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "c1");
  assert_eq!(messages[2].role, Role::Tool);
  assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
}

#[test]
fn multi_round_turn_keeps_each_assistant_with_its_own_results() {
  let round = |id: &str, text: &str, out: &str| Round {
    notes: Vec::new(),
    assistant: Assistant {
      custom: Vec::new(),
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
    custom: Vec::new(),
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
    custom: Vec::new(),
    user: Some("pick".to_string()),
    notes: vec![],
    rounds: vec![Round {
      notes: Vec::new(),
      assistant: Assistant {
        custom: Vec::new(),
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
  assert_eq!(
    parsed.pending_question().expect("pending").questions,
    questions
  );
  let messages = parsed.request_messages();
  assert_eq!(messages.len(), 3);
  assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "c1");
  assert_eq!(messages[2].role, Role::Tool);
  assert_eq!(messages[2].content.as_deref(), Some(AWAITING_ANSWER));

  parsed
    .answer_question(vec![(1, "blue".to_string())])
    .expect("answer");
  assert!(parsed.pending_question().is_none());
  let messages = parsed.request_messages();
  assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "c1");
  assert!(
    messages[2]
      .content
      .as_deref()
      .unwrap()
      .contains("\"answer\":\"blue\""),
    "{:?}",
    messages[2].content
  );
  let (mut reread, report) = parse(&parsed.render());
  assert!(report.is_clean(), "{report:?}");
  assert!(reread.pending_question().is_none());
  assert!(
    reread.answer_question(vec![]).is_err(),
    "nothing left to answer"
  );
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
  session
    .answer_question(vec![(1, "yes".to_string())])
    .expect("answer");
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
    again
      .turns
      .last()
      .unwrap()
      .rounds
      .last()
      .unwrap()
      .assistant
      .text
      .as_deref(),
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
  assert!(
    !rendered.contains("\r"),
    "structure is lf-only: {rendered:?}"
  );
  assert!(rendered.ends_with("hello\n"), "{rendered:?}");
  assert_eq!(parse(&rendered).0.input, "hello\n", "stable");
}

#[test]
fn toml_fence_round_trips_through_the_parser() {
  let config = SessionConfig {
    provider: Some("openrouter".to_string()),
    headers: BTreeMap::new(),
    burst_max_turns: None,
    tell_model_context_size: None,
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
fn the_fence_can_carry_a_key_and_survives_the_round_trip() {
  let config = SessionConfig {
    provider: Some("openrouter".to_string()),
    // The lazy path: an inline table with a literal key, plus an awkward value that has to be
    // escaped or the fence stops parsing -- and an unparseable fence is never regenerated, so
    // every later edit to that transcript's config would be silently frozen.
    headers: BTreeMap::from([
      (
        "Authorization".to_string(),
        "Bearer sk-or-v1-\"quoted\"".to_string(),
      ),
      ("HTTP-Referer".to_string(), "http://localhost".to_string()),
    ]),
    model: Some("qwen/qwen3-235b-a22b".to_string()),
    thinking: Thinking::Auto,
    ..Default::default()
  };
  let rendered = toml_fence(&config);
  assert!(
    rendered.contains("provider = \"openrouter\"\n"),
    "{rendered}"
  );
  assert!(
    rendered.contains(r#"headers = { "Authorization" = "Bearer sk-or-v1-\"quoted\"", "HTTP-Referer" = "http://localhost" }"#),
    "{rendered}"
  );
  assert_eq!(fence_config(&rendered).unwrap().unwrap(), config);
}

/// A provider name or model containing a quote used to write a fence that would not parse -- and
/// an unparseable fence is never regenerated, so every later edit to that transcript's config
/// would be frozen with no indication why.
#[test]
fn awkward_provider_and_model_names_survive_a_round_trip() {
  let config = SessionConfig {
    provider: Some("say\"hi".to_string()),
    model: Some("back\\slash".to_string()),
    thinking: Thinking::Auto,
    ..Default::default()
  };
  let rendered = toml_fence(&config);
  assert!(
    !fence_config(&rendered)
      .unwrap()
      .unwrap()
      .provider
      .as_deref()
      .unwrap()
      .is_empty(),
    "{rendered}"
  );
  assert_eq!(fence_config(&rendered).unwrap().unwrap(), config);
}

#[test]
fn the_fence_carries_the_provider_alongside_the_model() {
  let config = SessionConfig {
    provider: Some("openrouter".to_string()),
    headers: BTreeMap::new(),
    burst_max_turns: None,
    tell_model_context_size: None,
    model: Some("qwen3.8-27b".to_string()),
    thinking: Thinking::Auto,
    tools: Some(
      crate::flow::DEFAULT_TOOLS
        .iter()
        .map(|t| (*t).to_string())
        .collect::<Vec<_>>(),
    ),
    ..Default::default()
  };
  let rendered = toml_fence(&config);
  assert!(
    rendered.contains("provider = \"openrouter\"\nmodel = \"qwen3.8-27b\"\n"),
    "the provider is the outer choice and belongs above the model: {rendered}"
  );
  assert_eq!(fence_config(&rendered).unwrap().unwrap(), config);

  // A fence that never heard of providers still parses, with no provider claimed.
  let legacy = "```toml\nmodel = \"qwen3.8-27b\"\nthinking = \"auto\"\n```\n";
  assert_eq!(fence_config(legacy).unwrap().unwrap().provider, None);
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
  assert_eq!(
    round.assistant.calls[0].id, "c1",
    "ids are shortened on write"
  );
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
  let (parsed, report) = parse(&session.render());
  assert!(report.is_clean(), "{report:?}");
  assert!(!parsed.turn_in_progress());
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
  assert!(
    !report.is_clean(),
    "a corrupt header must not pass as clean"
  );
  let problems = report.problems().join("; ");
  assert!(problems.contains("call"), "{problems}");
  assert!(problems.contains("result"), "{problems}");
  assert!(
    problems.contains("turn"),
    "the report must locate the region: {problems}"
  );
  let round = &session.turns[0].rounds[0];
  assert!(
    round.assistant.calls.is_empty(),
    "an unparseable call is not invented"
  );
  assert!(
    !round.results[0].ok,
    "an unreadable result is never reported as a success"
  );
  assert!(
    round.results[0].output.contains("cannot be trusted"),
    "{:?}",
    round.results[0]
  );
}

#[test]
fn a_malformed_question_payload_is_reported() {
  let file = session_file(&container(TURN, &payload_region(QUESTION, "{oops")));
  let (session, report) = parse(&file);
  assert!(!report.is_clean());
  assert!(
    report.problems()[0].contains("question"),
    "{:?}",
    report.problems()
  );
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
  let file = session_file(&container(
    TURN,
    &result_region("c1", "bash", true, 3, output),
  ));
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
    s.turns.push(Turn {
      user: Some("hi".into()),
      ..Turn::default()
    });
    s.open = Some(vec!["turn".into()]);
    cases.push((s.open.clone(), s));
  }
  {
    let mut s = Session::default();
    s.turns.push(Turn {
      user: Some("hi".into()),
      ..Turn::default()
    });
    s.begin_round();
    s.open = Some(vec!["turn".into(), "assistant".into()]);
    cases.push((s.open.clone(), s));
  }
  {
    let mut s = Session::default();
    s.turns.push(Turn {
      user: Some("hi".into()),
      ..Turn::default()
    });
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
  let fence =
    "```toml\nmodel = \"special-model\"\nhelp = \"\"\"\n<!-- slopfile:begin -->\n\"\"\"\n```\n";
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
    custom: Vec::new(),
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
  assert_eq!(messages[1].tool_calls.as_ref().unwrap()[0].id, "c1");
  assert!(messages[2].content.as_deref().unwrap().contains("yes"));
  assert!(
    session.answer_question(vec![]).is_err(),
    "nothing left to answer"
  );
}

#[test]
fn promoting_input_never_overwrites_an_earlier_user_message() {
  // An abort before the first token leaves a turn holding a user message and an empty round.
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
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
  assert_eq!(
    session.turns[1].user.as_deref(),
    Some("and also fix the lint")
  );
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
      Call {
        id: "".into(),
        name: "bash".into(),
        arguments: "{}".into(),
      },
      Call {
        id: "".into(),
        name: "bash".into(),
        arguments: "{}".into(),
      },
    ]
  };
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".into()),
    ..Turn::default()
  });
  session.begin_round();
  session.finish_round(
    calls(),
    vec![
      ToolOutcome {
        id: "".into(),
        name: "bash".into(),
        ok: true,
        ms: 1,
        output: "one".into(),
      },
      ToolOutcome {
        id: "".into(),
        name: "bash".into(),
        ok: true,
        ms: 1,
        output: "two".into(),
      },
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
  // A question stops the round early, so later calls have no outcome of their own.
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".into()),
    ..Turn::default()
  });
  session.begin_round();
  session.finish_round(
    vec![
      Call {
        id: "c1".into(),
        name: "question".into(),
        arguments: "{}".into(),
      },
      Call {
        id: "c2".into(),
        name: "bash".into(),
        arguments: "{}".into(),
      },
    ],
    vec![ToolOutcome {
      id: "c1".into(),
      name: "question".into(),
      ok: true,
      ms: 1,
      output: "waiting for an answer".into(),
    }],
  );
  let messages = session.request_messages();
  let answered: Vec<&str> = messages
    .iter()
    .filter_map(|m| m.tool_call_id.as_deref())
    .collect();
  assert_eq!(answered, vec!["c1", "c2"], "both calls are answered");
  assert!(
    messages[3]
      .content
      .as_deref()
      .unwrap()
      .contains("not executed"),
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
  assert!(
    !report.is_clean(),
    "a note at the top level is lost otherwise"
  );
  assert!(
    report.problems()[0].contains("out of place"),
    "{:?}",
    report.problems()
  );

  let two_texts = session_file(&container(
    TURN,
    &container(
      ASSISTANT,
      &format!(
        "{}{}",
        region(TEXT, "first version"),
        region(TEXT, "second")
      ),
    ),
  ));
  let (session, report) = parse(&two_texts);
  assert!(
    !report.is_clean(),
    "the first text would be silently replaced"
  );
  assert!(
    report.problems()[0].contains("second `text`"),
    "{:?}",
    report.problems()
  );
  assert_eq!(
    session.turns[0].rounds[0].assistant.text.as_deref(),
    Some("first version"),
    "the first body is kept until the file is fixed"
  );

  let nested = session_file(&container(TURN, &container(TURN, &region(USER, "inner"))));
  let (_, report) = parse(&nested);
  assert!(!report.is_clean(), "a turn inside a turn is lost otherwise");
  assert!(
    report.problems()[0].contains("out of place"),
    "{:?}",
    report.problems()
  );

  let two_users = session_file(&container(
    TURN,
    &format!("{}{}", region(USER, "one"), region(USER, "two")),
  ));
  let (_, report) = parse(&two_users);
  assert!(
    !report.is_clean(),
    "a second user region would be overwritten"
  );
  assert!(
    report.problems()[0].contains("second `user`"),
    "{:?}",
    report.problems()
  );
}

#[test]
fn a_model_note_is_part_of_the_assistant_message_that_wrote_it() {
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".to_string()),
    notes: vec![],
    rounds: vec![Round {
      notes: vec![Note {
        by: NoteBy::Model,
        text: "remember: the user wants tests".to_string(),
      }],
      assistant: Assistant {
        custom: Vec::new(),
        reasoning: None,
        text: Some("Understood.".to_string()),
        calls: vec![],
      },
      results: vec![],
    }],
    question: None,
  });
  let messages = session.request_messages();
  assert_eq!(messages.len(), 2, "user + one assistant: {messages:?}");
  assert_eq!(messages[0].role, Role::User);
  assert_eq!(messages[1].role, Role::Assistant);
  let content = messages[1].content.as_deref().unwrap();
  assert!(content.contains("remember"), "{content}");
  assert!(content.contains("Understood"), "{content}");
}

#[test]
fn a_model_note_is_sent_before_the_result_of_the_round_that_wrote_it() {
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".to_string()),
    notes: vec![],
    rounds: vec![Round {
      notes: vec![Note {
        by: NoteBy::Model,
        text: "remember: the next step is to re-run the tests".to_string(),
      }],
      assistant: Assistant {
        custom: Vec::new(),
        reasoning: None,
        text: None,
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
        ms: 0,
        output: "src".to_string(),
      }],
    }],
    question: None,
  });
  let messages = session.request_messages();
  let note_at = messages
    .iter()
    .position(|m| m.content.as_deref() == Some("remember: the next step is to re-run the tests"))
    .expect("the note is still in the request");
  let tool_at = messages
    .iter()
    .position(|m| m.role == Role::Tool)
    .expect("the result is in the request");
  assert!(note_at < tool_at, "{messages:?}");
  assert_eq!(
    messages.last().unwrap().role,
    Role::Tool,
    "the tool result must be the prompt, not the model's own note: {messages:?}"
  );
  assert!(session.needs_generation());
}

#[test]
fn a_model_note_does_not_trail_the_empty_round_a_generation_starts_with() {
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".to_string()),
    notes: vec![],
    rounds: vec![Round {
      notes: vec![Note {
        by: NoteBy::Model,
        text: "remember: the findings are in the note".to_string(),
      }],
      assistant: Assistant {
        custom: Vec::new(),
        reasoning: None,
        text: None,
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
        ms: 0,
        output: "src".to_string(),
      }],
    }],
    question: None,
  });
  // What the daemon does before asking again: a live turn gets a fresh, still empty round.
  session.begin_round();

  let messages = session.request_messages();
  assert_eq!(
    messages.last().unwrap().role,
    Role::Tool,
    "the empty tail round must not push the model's note to the end: {messages:?}"
  );
  assert!(
    messages.iter().any(|m| m.role == Role::Assistant
      && m.content.as_deref() == Some("remember: the findings are in the note")),
    "the note is still there, just not last: {messages:?}"
  );
}

#[test]
fn a_model_note_with_no_round_goes_after_the_user() {
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("carry on".to_string()),
    notes: vec![Note {
      by: NoteBy::Model,
      text: "remember: the findings are in the note".to_string(),
    }],
    rounds: Vec::new(),
    question: None,
  });

  let messages = session.request_messages();
  assert_eq!(messages.len(), 2, "{messages:?}");
  assert_eq!(messages[0].role, Role::User);
  assert_eq!(messages[0].content.as_deref(), Some("carry on"));
  assert_eq!(messages[1].role, Role::Assistant);
}

#[test]
fn a_model_note_does_not_move_when_later_rounds_are_added() {
  fn key(
    m: &ChatMessage,
  ) -> (
    Role,
    Option<String>,
    Option<String>,
    Option<Vec<ToolCall>>,
    Option<String>,
  ) {
    (
      m.role,
      m.content.clone(),
      m.reasoning_content.clone(),
      m.tool_calls.clone(),
      m.tool_call_id.clone(),
    )
  }

  let round = |id: &str, text: &str, out: &str| Round {
    notes: Vec::new(),
    assistant: Assistant {
      custom: Vec::new(),
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

  let mut first_round = round("c0", "first", "one");
  first_round.notes.push(Note {
    by: NoteBy::Model,
    text: "remember: the plan is in the note".to_string(),
  });

  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".to_string()),
    notes: vec![],
    rounds: vec![first_round],
    question: None,
  });
  session.begin_round();
  let first = session.request_messages();

  session.turns[0].rounds[1] = round("c1", "second", "two");
  session.begin_round();
  let second = session.request_messages();

  assert!(second.len() > first.len(), "{first:?} {second:?}");
  for (before, after) in first.iter().zip(&second) {
    assert_eq!(
      key(before),
      key(after),
      "the note must stay where it was written: {first:?} vs {second:?}"
    );
  }
}

#[test]
fn a_model_note_renders_between_its_round_and_that_rounds_results() {
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".to_string()),
    notes: vec![],
    rounds: vec![Round {
      notes: vec![Note {
        by: NoteBy::Model,
        text: "tried X: no results".to_string(),
      }],
      assistant: Assistant {
        custom: Vec::new(),
        reasoning: None,
        text: Some("searching".to_string()),
        calls: vec![Call {
          id: "c1".to_string(),
          name: "bash".to_string(),
          arguments: r#"{"command":"search X"}"#.to_string(),
        }],
      },
      results: vec![ToolOutcome {
        id: "c1".to_string(),
        name: "bash".to_string(),
        ok: true,
        ms: 0,
        output: "nothing useful".to_string(),
      }],
    }],
    question: None,
  });

  let rendered = session.render();
  let call = rendered.find("slopfile:call").expect("call");
  let note = rendered.find("tried X").expect("note");
  let result = rendered.find("slopfile:result").expect("result");
  assert!(call < note && note < result, "{rendered}");

  let (parsed, report) = parse(&rendered);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(
    parsed.turns[0].rounds[0].notes[0].text,
    "tried X: no results"
  );
  assert_eq!(parsed.render(), rendered, "rendering is idempotent");
}

#[test]
fn a_note_added_while_a_call_is_pending_lands_before_that_calls_result() {
  // Exactly what the batch writes: the assistant region is closed, the note follows it, and the
  // result does not exist yet because the call is still running.
  let file = session_file(&container(
    TURN,
    &format!(
      "{}{}{}",
      region(USER, "go"),
      container(
        ASSISTANT,
        &format!(
          "{}{}",
          region(TEXT, "searching"),
          call_region("c1", "bash", r#"{"command":"search X"}"#)
        )
      ),
      note_region("model", "tried X: no results")
    ),
  ));
  let (mut session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(session.turns[0].rounds[0].notes.len(), 1);

  // The daemon attaches the result after adopting the edit.
  session.finish_round(
    vec![Call {
      id: "c1".into(),
      name: "bash".into(),
      arguments: r#"{"command":"search X"}"#.into(),
    }],
    vec![ToolOutcome {
      id: "c1".into(),
      name: "bash".into(),
      ok: true,
      ms: 0,
      output: "nothing useful".into(),
    }],
  );

  let rendered = session.render();
  let note = rendered.find("tried X").unwrap();
  let result = rendered.find("slopfile:result").unwrap();
  assert!(note < result, "{rendered}");

  let messages = session.request_messages();
  assert_eq!(messages.last().unwrap().role, Role::Tool, "{messages:?}");
}

#[test]
fn a_legacy_tail_note_attaches_to_the_last_round_on_reparse() {
  // What the old renderer wrote: a model note at the end of the turn, after the results.
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".to_string()),
    notes: vec![Note {
      by: NoteBy::Model,
      text: "tried X: no results".to_string(),
    }],
    rounds: vec![Round {
      notes: Vec::new(),
      assistant: Assistant {
        custom: Vec::new(),
        reasoning: None,
        text: None,
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
        ms: 0,
        output: "src".to_string(),
      }],
    }],
    question: None,
  });

  let legacy = session.render();
  let (parsed, report) = parse(&legacy);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(
    parsed.turns[0].rounds[0].notes[0].text,
    "tried X: no results"
  );
  assert!(parsed.turns[0].notes.is_empty());

  let stabilised = parsed.render();
  assert!(
    stabilised.find("tried X").unwrap() < stabilised.find("slopfile:result").unwrap(),
    "the note moves back before the result on the first render: {stabilised}"
  );
  assert_eq!(parse(&stabilised).0.render(), stabilised);
}

#[test]
fn rewinding_a_round_drops_its_calls_too() {
  let mut session = Session::default();
  session.turns.push(Turn {
    custom: Vec::new(),
    user: Some("go".into()),
    ..Turn::default()
  });
  session.begin_round();
  session.push_text("half");
  session.finish_round(
    vec![Call {
      id: "c1".into(),
      name: "bash".into(),
      arguments: "{}".into(),
    }],
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

#[test]
fn every_thinking_level_round_trips_through_the_fence() {
  for want in [
    Thinking::Auto,
    Thinking::None,
    Thinking::Low,
    Thinking::Medium,
    Thinking::High,
    Thinking::Xhigh,
  ] {
    let file =
      format!("```toml\nthinking = \"{want}\"\n```\n{BEGIN_LINE}<!-- slopfile:input -->\n");
    let (session, report) = parse(&file);
    assert!(report.is_clean(), "{want:?} should parse clean: {report:?}");
    assert_eq!(session.thinking(), want);
    // And the fence keeps saying what the session is actually using, instead of
    // re-stamping a hardcoded value that would lie after a hand edit.
    assert!(
      session.render().contains(&format!("thinking = \"{want}\"")),
      "{want:?} must survive render: {}",
      session.render()
    );
  }
}

#[test]
fn an_unknown_thinking_level_is_rejected_and_names_the_valid_ones() {
  // The regression guard for the knob being silently inert: `Thinking` used to be
  // `#[serde(other)]`, so "meduim" quietly became `auto` and nobody ever found out.
  let err = toml::from_str::<SessionConfig>("thinking = \"meduim\"")
    .expect_err("a typo must not be swallowed");
  let text = err.to_string();
  for valid in ["auto", "none", "low", "medium", "high", "xhigh"] {
    assert!(
      text.contains(valid),
      "the error should list {valid}: {text}"
    );
  }
}

#[test]
fn an_unparseable_fence_falls_back_without_bricking_the_session() {
  // Hand-edited fences are TOML-ish prose. They must stay driveable -- that is what
  // `a_hand_edited_fence_survives_a_save_even_behind_prose` protects -- and the settings in them
  // are simply ignored, loudly, by the warning in parse().
  let file = format!(
    "```toml\nthinking = \"meduim\"\n```\n{BEGIN_LINE}<!-- slopfile:system -->\nsys\n\
     <!-- /slopfile -->\n<!-- slopfile:input -->\n"
  );
  let (session, report) = parse(&file);
  assert!(
    report.is_clean(),
    "a bad fence is a warning, not a refusal: {report:?}"
  );
  assert_eq!(session.thinking(), Thinking::Auto);
  assert!(
    session.render().contains("thinking = \"meduim\""),
    "the hand-edited fence must survive verbatim: {}",
    session.render()
  );
}

#[test]
fn a_session_with_no_thinking_key_is_auto_and_sends_nothing() {
  let file = format!("```toml\nmodel = \"m\"\n```\n{BEGIN_LINE}<!-- slopfile:input -->\n");
  let (session, report) = parse(&file);
  assert!(report.is_clean(), "{report:?}");
  assert_eq!(session.thinking(), Thinking::Auto);
  assert_eq!(Thinking::Auto.wire_value(), None);
}

fn good_edit() -> String {
  session_file(&format!(
    "{}{}",
    region(SYSTEM, "sys"),
    container(TURN, &region(USER, "hi"))
  ))
}

#[test]
fn a_sound_edit_passes() {
  assert!(adopt(&good_edit()).is_ok());
}

#[test]
fn edits_must_keep_the_toml_fence() {
  let err = adopt("<!-- slopfile:begin -->\n<!-- slopfile:system -->\ns\n<!-- /slopfile -->\n")
    .expect_err("no fence");
  assert!(err.contains("toml header"), "{err}");
}

#[test]
fn edits_must_keep_the_system_region() {
  let err = adopt(&session_file(&container(TURN, ""))).expect_err("no system region");
  assert!(err.contains("system"), "{err}");
}

#[test]
fn edits_must_keep_the_structure_sound() {
  let err = adopt(&session_file("<!-- /slopfile -->\n")).expect_err("stray close");
  assert!(err.contains("never opened"), "{err}");

  let unclosed = session_file(&format!(
    "<!-- slopfile:{SYSTEM} -->\ns\n<!-- /slopfile -->\n<!-- slopfile:{TURN} -->\n<!-- slopfile:{ASSISTANT} -->\n<!-- slopfile:{TEXT} -->\nleak\n<!-- slopfile:{USER} -->\nnope\n<!-- /slopfile -->\n"
  ));
  let err = adopt(&unclosed).expect_err("unclosed leaf");
  assert!(err.contains("left unclosed"), "{err}");

  let err = adopt(&session_file(&format!(
    "{}<!-- slopfile:mystery -->\nx\n<!-- /slopfile -->\n",
    region(SYSTEM, "s")
  )))
  .expect_err("unknown region");
  assert!(err.contains("unknown region"), "{err}");
}

#[test]
fn adopt_takes_a_pruned_edit() {
  let pruned = good_edit().replace("hi\n", "");
  let edited = adopt(&pruned).expect("adopt");
  assert_eq!(
    edited.turns[0].user.as_deref(),
    Some(""),
    "the user text is gone"
  );
  assert_eq!(edited.system, "sys");
}

#[test]
fn adopt_refuses_an_unusable_edit() {
  assert!(adopt("").is_err());
  assert!(adopt("everything is gone\n").is_err());
  let no_system = session_file(&escape("no system region here\n"));
  assert!(adopt(&no_system).is_err());
}
