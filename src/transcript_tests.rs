use crate::transcript::{
  abort_marker_present, assistant_section_messages, draft_tail, find_done_marker, header_config,
  is_abort_line, parse_file, reassemble, strip_live_line, tail_sections, tail_user_text,
};
use crate::types::Role;

  fn sample() -> String {
    r#"```toml
model = "qwen3.8-27b"
thinking = "auto"
tools = ["read","bash"]
```
# Session
You are a coding agent.

### You
Hello, agent.

### Assistant
Hi there.

### You
Show me the files.

### Assistant
Let me check.
<!-- thinking -->
I will ls first.
<!-- /thinking -->
<!-- tool:bash {"cmd":"ls"} -->
src/
docs/
<!-- result:bash ok=42ms -->
Here is the listing.
<!-- @done -->
"#
    .to_string()
  }

  #[test]
  fn header_parsed_from_first_fence_only() {
    let cfg = header_config(&sample());
    assert_eq!(cfg.model.as_deref(), Some("qwen3.8-27b"));
    assert_eq!(
      cfg.tools.as_deref(),
      Some(["read".to_string(), "bash".to_string()].as_slice())
    );

    let txt2 = format!(
      "{}\n### Assistant\n```toml\nmodel = \"hacked\"\n```\nThis is code.\n",
      sample()
    );
    let cfg2 = header_config(&txt2);
    assert_eq!(cfg2.model.as_deref(), Some("qwen3.8-27b"));
  }

  #[test]
  fn system_prompt_extracted() {
    let pf = parse_file(&sample());
    assert!(pf.history[0].content.as_deref().unwrap().contains("You are a coding agent."));
    assert_eq!(pf.history[0].role, Role::System);
  }

  #[test]
  fn history_round_trip_with_tools_and_thinking() {
    let pf = parse_file(&sample());
    let roles: Vec<Role> = pf.history.iter().map(|m| m.role).collect();
    assert_eq!(
      roles,
      vec![
        Role::System,
        Role::User,
        Role::Assistant,
        Role::User,
        Role::Assistant,
        Role::Tool,
        Role::Assistant
      ]
    );

    let asst_with_call = pf
      .history
      .iter()
      .find(|m| m.tool_calls.is_some())
      .expect("tool call");
    assert_eq!(asst_with_call.tool_calls.as_ref().unwrap()[0].name, "bash");
    assert!(asst_with_call
      .reasoning_content
      .as_deref()
      .unwrap()
      .contains("I will ls"));

    let tool_msg = pf.history.iter().find(|m| m.role == Role::Tool).expect("tool msg");
    assert!(tool_msg.content.as_deref().unwrap().contains("src/"));
    assert_eq!(
      asst_with_call.tool_calls.as_ref().unwrap()[0].id,
      tool_msg.tool_call_id.as_deref().unwrap()
    );
  }

  #[test]
  fn dangling_detected() {
    let mut txt = sample();
    txt.push_str("### You\nNext.\n");
    assert!(parse_file(&txt).dangling.is_none());

    let mut txt2 = sample();
    txt2.push_str("### Assistant\npartial output without done\n");
    let pf = parse_file(&txt2);
    assert!(pf.dangling.is_some());
    let d = pf.dangling.as_ref().unwrap();
    assert!(d.partial_text.contains("partial output without done"));
    assert!(d.resume_offset > 0);
  }

  #[test]
  fn dangling_partial_replays_as_assistant_messages() {
    let partial = "<!-- thinking -->\nlet me think about this\n<!-- /thinking -->\nContinue the work.\n";
    let msgs = assistant_section_messages(partial);
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!(m.role, Role::Assistant);
    assert!(m
      .content
      .as_deref()
      .is_some_and(|c| c.contains("Continue the work")));
    assert!(m
      .reasoning_content
      .as_deref()
      .is_some_and(|r| r.contains("let me think about this")));
  }

  #[test]
  fn dangling_partial_prose_only_is_content() {
    let msgs = assistant_section_messages("still working on it\n");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::Assistant);
    assert!(msgs[0]
      .content
      .as_deref()
      .is_some_and(|c| c.starts_with("still working")));
  }

  #[test]
  fn status_line_excluded() {
    let mut txt = sample();
    txt.push_str("\n<!-- slopfile:live {\"phase\":\"idle\"} -->\n");
    let pf = parse_file(&txt);
    assert!(!pf.tail.contains("slopfile:live"));
    assert!(!pf.tail.contains("{\"phase\":\"idle\"}"));
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:live"))));
  }

  #[test]
  fn aborted_marker_bounds_the_turn_like_done() {
    let txt = r#"```toml
model = "qwen3.8-27b"
```
# Sm
Sys.

### You
Question?

### Assistant
Let me check.
<!-- thinking -->
will ls
<!-- /thinking -->
<!-- tool:bash {"cmd":"ls"} -->
src/
<!-- result:bash ok=1ms -->
partial reply cut off mid
<!-- /thinking -->
<!-- slopfile:aborted -->
<!-- slopfile:live {"phase":"idle","model":"qwen3.8-27b"} -->
"#;

    let pf = parse_file(txt);
    assert!(pf.dangling.is_none(), "aborted turn must not be resumable");
    assert!(pf.tail.trim().is_empty(), "no tail below aborted turn");
    assert!(find_done_marker(txt).is_some(), "aborted marker bounds history");
    assert!(txt.contains("<!-- slopfile:aborted -->"), "marker stays visible");
    let last = pf.history.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:aborted"))));
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:live"))));
  }

  #[test]
  fn status_line_after_closed_tool_block_is_not_history() {
    let txt = "\
```toml
model = \"qwen3.8-27b\"
```
# Smoke
You are a helpful test agent.

### You
Question?

### Assistant
<!-- thinking -->
Which questions?
<!-- /thinking -->
I will ask.
<!-- tool:question {\"desc\":\"x\"} -->
answers json
<!-- result:question ok=ask -->

<!-- slopfile:live {\"phase\":\"waiting_answer\",\"model\":\"qwen3.8-27b\"} -->
";
    let pf = parse_file(txt);
    assert_eq!(pf.history.last().map(|m| m.role), Some(Role::Tool));
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("slopfile:live"))));
    assert!(pf.dangling.is_none());
  }

  #[test]
  fn tail_is_below_done() {
    let mut txt = sample();
    txt.push_str("NEW DRAFT HERE\n");
    let pf = parse_file(&txt);
    assert!(pf.tail.contains("NEW DRAFT HERE"));
    assert!(!pf
      .history
      .iter()
      .any(|m| m.content.as_deref().is_some_and(|c| c.contains("NEW DRAFT"))));
  }

  #[test]
  fn done_marker_found() {
    assert!(find_done_marker(&sample()).is_some());
    assert!(parse_file(&sample()).dangling.is_none());
  }

  #[test]
  fn no_header_means_system_prompt_is_prefix() {
    let txt = "Direct system text.\n\n### You\nhi\n";
    let pf = parse_file(txt);
    assert_eq!(pf.history[0].role, Role::System);
    assert!(pf.history[0]
      .content
      .as_deref()
      .unwrap()
      .contains("Direct system text"));
  }

  #[test]
  fn unterminated_tool_block_is_resumed() {
    let mut txt = sample();
    txt.push_str("### Assistant\n<!-- tool:bash {\"cmd\":\"sleep\"} -->\n");
    let pf = parse_file(&txt);
    assert!(pf.history.iter().any(|m| m.tool_calls.is_some()));
  }

  #[test]
  fn questions_in_tail_are_detected() {
    let mut txt = sample();
    txt.push_str("\n### You\nwhat do you think?\n");
    txt.push_str(&crate::questions::render_block(&[crate::questions::Question {
      id: 1,
      question: "red or blue".to_string(),
      answers: vec!["red".into(), "blue".into()],
      default: None,
    }]));
    let pf = parse_file(&txt);
    assert_eq!(pf.pending_questions.len(), 1);
    assert_eq!(pf.pending_questions[0].questions[0].question, "red or blue");
    assert!(pf.tail.contains("QUESTIONS"));
  }

  #[test]
  fn markers_inside_closed_fence_are_not_structural() {
    let txt = "\
### Assistant
```md
<!-- tool:bash {\"cmd\":\"ls\"} -->
some code
```
### Assistant
real reply
";
    let pf = parse_file(txt);
    let asst_msgs = pf
      .history
      .iter()
      .filter(|m| m.role == Role::Assistant)
      .count();
    assert_eq!(asst_msgs, 2);
    assert!(!pf
      .history
      .iter()
      .any(|m| m.tool_calls.is_some()), "fenced tool marker must not open a block");
    let last = pf.history.last().unwrap();
    assert_eq!(last.content.as_deref().unwrap().trim(), "real reply");
  }

  #[test]
  fn only_level_three_headings_transition_sections() {
    let txt = "\
### You
first
## You
draft not a transition
# Assistant
more draft
#### You
still draft
### Assistant
reply
### You
second
";
    let pf = parse_file(txt);
    let roles: Vec<Role> = pf.history.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
    let users: Vec<&str> = pf
      .history
      .iter()
      .filter(|m| m.role == Role::User)
      .flat_map(|m| m.content.as_deref())
      .collect();
    assert!(users[0].contains("first"));
    assert!(users[0].contains("## You"), "lower-level headings stay in the draft");
    assert!(users[0].contains("# Assistant"), "lower-level headings stay in the draft");
    assert!(users[1].contains("second"));
  }

  #[test]
  fn thinking_markers_only_toggle_inside_assistant_prose() {
    let txt = "\
### You
<!-- thinking -->
user pasted a marker
#### /thinking more text
### Assistant
plain
### You
<!-- /thinking -->
still user text
";
    let pf = parse_file(txt);
    let asst = pf
      .history
      .iter()
      .find(|m| m.role == Role::Assistant)
      .unwrap();
    assert_eq!(asst.reasoning_content, None, "markers outside assistant prose never toggle");
    assert!(asst.content.as_deref().unwrap().contains("plain"));
    let users: Vec<&str> = pf
      .history
      .iter()
      .filter(|m| m.role == Role::User)
      .flat_map(|m| m.content.as_deref())
      .collect();
    assert!(users[0].contains("<!-- thinking -->"), "marker stays as user text");
    assert!(users[0].contains("user pasted a marker"));
    assert_eq!(users[0].find("There is"), None);
  }

  #[test]
  fn thinking_markers_inside_tool_block_are_output_text() {
    let txt = "\
### Assistant
<!-- tool:bash {\"cmd\":\"ls\"} -->
<!-- thinking -->
<!-- /thinking -->
out
<!-- result:bash ok=1ms -->
done
";
    let pf = parse_file(txt);
    let tool = pf.history.iter().find(|m| m.role == Role::Tool).unwrap();
    let c = tool.content.as_deref().unwrap();
    assert!(c.contains("<!-- thinking -->"), "marker is tool output");
    assert!(c.contains("<!-- /thinking -->"), "marker is tool output");
    assert!(c.contains("out"));
    assert_eq!(pf.history.last().unwrap().role, Role::Assistant);
  }

  #[test]
  fn mismatched_result_name_does_not_close_the_block() {
    let txt = "\
### Assistant
<!-- tool:bash {\"cmd\":\"ls\"} -->
src/
<!-- result:grep ok=1ms -->
docs/
<!-- result:bash ok=42ms -->
done
";
    let pf = parse_file(txt);
    let tool = pf.history.iter().find(|m| m.role == Role::Tool).unwrap();
    let c = tool.content.as_deref().unwrap();
    assert!(c.contains("src/"));
    assert!(c.contains("<!-- result:grep ok=1ms -->"), "mismatched result stays in output");
    assert!(c.contains("docs/"));
    assert!(!c.contains("<!-- result:bash ok=42ms -->"), "matching result is structural");
    assert_eq!(pf.history.last().unwrap().role, Role::Assistant);
    assert_eq!(pf.history.last().unwrap().content.as_deref(), Some("done\n"));
  }

  #[test]
  fn unclosed_fence_below_next_turn_is_not_retroactive() {
    let txt = "\
### Assistant
```json
{\"x\": 1}
<!-- @done -->
### You
what now?
";
    let pf = parse_file(txt);
    assert!(find_done_marker(txt).is_some(), "@done still a done marker");
    let msgs = tail_sections(&pf.tail);
    let last = msgs.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(last.content.as_deref().unwrap().contains("what now?"));
  }

  #[test]
  fn crlf_file_parses_without_panic_and_exact_history() {
    let txt = "\
### You\r
Hello ❓ friend.\r
\r
### Assistant\r
Reply ❓ ok.\r
<!-- @done -->\r
QUESTIONS\r
\r
| # | Question | Options |\r
| 1 | red or blue | red, blue |\r
ANSWER: (numbers or text, comma separated)\r
> \r
";
    let pf = parse_file(txt);
    assert_eq!(pf.history[0].role, Role::User);
    assert!(pf.history[0]
      .content
      .as_deref()
      .unwrap()
      .contains("Hello ❓ friend."));
    assert_eq!(pf.history[1].role, Role::Assistant);
    assert!(pf.history[1]
      .content
      .as_deref()
      .unwrap()
      .contains("Reply ❓ ok."));
    assert_eq!(pf.pending_questions.len(), 1);
    assert!(pf.tail.trim_start().starts_with("QUESTIONS"));
    assert!(pf.dangling.is_none());
  }

  #[test]
  fn abort_marker_is_line_based() {
    assert!(abort_marker_present("hi\n<!-- slopfile:abort -->\n"));
    assert!(abort_marker_present(
      "<!-- slopfile:live {\"x\":1} --><!-- slopfile:abort -->\n"
    ));
    assert!(!abort_marker_present(
      "47: - Abort: `<!-- slopfile:abort -->` marker line\n"
    ));
    assert!(is_abort_line("<!-- slopfile:abort -->"));
    assert!(!is_abort_line("  prose <!-- slopfile:abort --> mention"));
  }

  #[test]
  fn strip_live_line_removes_only_the_live_line() {
    let src = "a\nb\n<!-- slopfile:live {\"phase\":\"idle\"} -->\n";
    let (body, live) = strip_live_line(src);
    assert_eq!(body, "a\nb\n");
    let text = live.expect("live line removed");
    assert_eq!(text, "<!-- slopfile:live {\"phase\":\"idle\"} -->");
    assert_eq!(strip_live_line("plain\n").0, "plain\n");
  }

  #[test]
  fn strip_live_line_removes_all_live_lines_and_reports_last_offset() {
    let src = "a\n<!-- slopfile:live {\"phase\":\"idle\"} -->\nb\n<!-- slopfile:live {\"phase\":\"gen\"} -->\nc";
    let (body, live) = strip_live_line(src);
    let text = live.expect("last live line reported");
    assert_eq!(text, "<!-- slopfile:live {\"phase\":\"gen\"} -->");
    assert!(!body.contains("slopfile:live"), "all live lines removed");
    assert_eq!(body, "a\nb\nc\n");
  }

  #[test]
  fn reassemble_appends_extra_then_restores_live() {
    let out = reassemble("body\n", "MARKER\n", Some("LIVE"));
    assert_eq!(out, "body\nMARKER\nLIVE");
    assert_eq!(reassemble("body", "X\n", None), "body\nX\n");
  }

  #[test]
  fn tail_user_text_and_draft_tail_agree_on_plain_drafts() {
    let txt = format!("{}\nkeep running daemons\n", sample());
    let tail = tail_user_text(&txt).expect("tail found");
    assert!(tail.contains("keep running daemons"));
    assert_eq!(draft_tail(&txt).as_deref(), Some(tail.as_str()));
  }

  #[test]
  fn draft_tail_rejects_explicit_section_headings() {
    let txt = format!("{}\n### You\nnext\n", sample());
    assert!(draft_tail(&txt).is_none(), "heading-led tail is not a draft");
    assert!(tail_user_text(&txt).is_some(), "but it is still user content");
  }

