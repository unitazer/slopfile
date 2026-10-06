use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub id: u32,
    pub question: String,
    pub answers: Vec<String>,
    pub default: Option<String>,
}

pub const BLOCK_HEADER: &str = "QUESTIONS";

pub const SKIP: &str = "/skip";
/// The last line the harness writes, so anything after it is the human's, never the model's.
pub const ANSWER_PROMPT: &str = "ANSWER: (numbers or text, comma separated)\n";

pub fn question_from_tool_item(id: u32, v: &serde_json::Value) -> Question {
  let question = v
    .get("question")
    .and_then(|q| q.as_str())
    .unwrap_or("?")
    .to_string();
  let answers = v
    .get("answers")
    .and_then(|a| a.as_array())
    .map(|a| {
      a.iter()
        .map(|x| x.as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>()
    })
    .unwrap_or_default();
  let default = v
    .get("default")
    .and_then(|d| d.as_str())
    .map(String::from);
  Question {
    id,
    question,
    answers,
    default,
  }
}

pub fn render_block(qs: &[Question]) -> String {
    let mut out = String::new();
    out.push_str(BLOCK_HEADER);
    out.push('\n');
    out.push_str("| # | Question | Options |\n");
    out.push_str("|---|----------|---------|\n");
    for q in qs {
        let opts = q
            .answers
            .iter()
            .map(|a| a.replace('|', "\\|"))
            .collect::<Vec<_>>()
            .join(" # ");
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            q.id,
            q.question.replace('|', "\\|"),
            opts
        ));
    }
    out.push_str(ANSWER_PROMPT);
    out.push_str("> \n");
    out
}

/// Everything at or after the answer prompt, which is the part only the human writes.
fn after_answer_prompt(input: &str) -> &str {
  match input.rfind(ANSWER_PROMPT) {
    Some(at) => &input[at + ANSWER_PROMPT.len()..],
    None => input,
  }
}

pub fn answer_text(input: &str) -> Option<String> {
  // Anything at or after the answer prompt was written by the human; the model only ever writes
  // above it, so a "> 1" line inside its own question text cannot pass for an answer.
  let body = after_answer_prompt(input);
  let mut found = None;
  for line in body.lines() {
    let t = line.trim();
    let candidate = if let Some(rest) = t.strip_prefix('>') {
      rest.trim()
    } else if t == SKIP {
      SKIP
    } else {
      continue;
    };
    if !candidate.is_empty() {
      found = Some(candidate.to_string());
    }
  }
  found
}

pub fn is_skip(input: &str) -> bool {
  // Only the human's half of the region counts. A "/skip" line inside the model's own question
  // text would otherwise auto-resolve the question it just asked, with the defaults.
  after_answer_prompt(input).lines().any(|l| {
    let t = l.trim();
    t == SKIP || t.strip_prefix('>').map(str::trim) == Some(SKIP)
  })
}

pub fn parse_answers(questions: &[Question], answer_text: &str) -> Result<Vec<(u32, String)>, String> {
  let mut tokens: VecDeque<String> = answer_text
    .split([',', '\n'])
    .map(|t| t.trim())
    .filter(|t| !t.is_empty())
    .map(String::from)
    .collect();

  let mut out = Vec::new();
  for q in questions {
    let token = tokens
      .pop_front()
      .or_else(|| q.default.clone())
      .ok_or_else(|| format!("missing answer for question(s): {}", q.id))?;
    out.push((q.id, resolve_token(&token, q)));
  }
  Ok(out)
}

pub fn resolve_positional(
  questions: &[Question],
  answers: &[String],
) -> Result<Vec<(u32, String)>, String> {
  if answers.len() != questions.len() {
    return Err(format!(
      "failed to parse question answers, expected {} answers, got {}",
      questions.len(),
      answers.len()
    ));
  }
  Ok(questions
    .iter()
    .zip(answers.iter())
    .map(|(q, a)| (q.id, resolve_token(a.trim(), q)))
    .collect())
}

fn resolve_token(token: &str, q: &Question) -> String {
    if let Ok(k) = token.parse::<usize>()
        && (1..=q.answers.len()).contains(&k)
    {
        return q.answers[k - 1].clone();
    }
    token.to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn qs() -> Vec<Question> {
    vec![
      Question {
        id: 1,
        question: "red or blue".to_string(),
        answers: vec!["red".into(), "blue".into()],
        default: None,
      },
      Question {
        id: 2,
        question: "how long?".to_string(),
        answers: vec!["3".into(), "5".into(), "10".into()],
        default: Some("5".into()),
      },
    ]
  }

  #[test]
  fn render_block_ends_with_an_answer_prompt() {
    let rendered = render_block(&qs());
    assert!(rendered.contains("QUESTIONS"));
    assert!(rendered.contains("| 1 | red or blue | red # blue |"));
    assert!(rendered.contains("| 2 | how long? | 3 # 5 # 10 |"));
    assert!(rendered.ends_with("> \n"));
  }

  #[test]
  fn render_block_escapes_pipes() {
    let q = vec![Question {
      id: 1,
      question: "a | b".to_string(),
      answers: vec!["x | y".to_string()],
      default: None,
    }];
    assert!(render_block(&q).contains("a \\| b"));
  }

  #[test]
  fn answer_text_finds_the_prompt_line() {
    let mut input = render_block(&qs());
    assert_eq!(answer_text(&input), None, "an empty prompt is not an answer");
    input.push_str("1\n");
    assert_eq!(answer_text(&input), None);
    input.push_str("> 1, 3\n");
    assert_eq!(answer_text(&input).as_deref(), Some("1, 3"));
  }

  #[test]
  fn answer_text_takes_the_last_answer_line() {
    let mut input = render_block(&qs());
    input.push_str("> 1\n");
    input.push_str("> 2\n");
    assert_eq!(answer_text(&input).as_deref(), Some("2"));
  }

  #[test]
  fn skip_is_recognized_bare_or_prompted() {
    assert!(is_skip("/skip"));
    assert!(is_skip("> /skip"));
    assert!(is_skip("noise\n> /skip\n"));
    assert!(!is_skip("> /skippy boots"));
    assert!(!is_skip(""));
  }

  #[test]
  fn parse_answers_uses_indices() {
    assert_eq!(
      parse_answers(&qs(), "1\n2").expect("answers"),
      vec![(1, "red".to_string()), (2, "5".to_string())]
    );
  }

  #[test]
  fn parse_answers_uses_verbatim_text() {
    assert_eq!(
      parse_answers(&qs(), "purple\n4").expect("answers"),
      vec![(1, "purple".to_string()), (2, "4".to_string())]
    );
  }

  #[test]
  fn parse_answers_mixed() {
    assert_eq!(
      parse_answers(&qs(), "2, 3").expect("answers"),
      vec![(1, "blue".to_string()), (2, "10".to_string())]
    );
  }

  #[test]
  fn parse_answers_uses_default_when_short() {
    assert_eq!(
      parse_answers(&qs(), "1").expect("default fallback"),
      vec![(1, "red".to_string()), (2, "5".to_string())]
    );
  }

  #[test]
  fn parse_answers_reports_missing() {
    let no_defaults = vec![
      Question {
        id: 1,
        question: "a?".into(),
        answers: vec!["x".into()],
        default: None,
      },
      Question {
        id: 2,
        question: "b?".into(),
        answers: vec!["y".into()],
        default: None,
      },
    ];
    let err = parse_answers(&no_defaults, "1").expect_err("must fail");
    assert!(err.contains("missing answer for question(s)"), "{err}");
  }

  #[test]
  fn parse_answers_out_of_range_index_uses_verbatim() {
    let answers = parse_answers(&qs(), "9\nx").expect("fallback");
    assert_eq!(answers[0].1, "9");
  }

  #[test]
  fn resolve_positional_is_strict_about_count() {
    let err = resolve_positional(&qs(), &["1".to_string()]).expect_err("count mismatch");
    assert_eq!(
      err,
      "failed to parse question answers, expected 2 answers, got 1"
    );
    let err = resolve_positional(&qs(), &[]).expect_err("no answers");
    assert!(err.contains("expected 2 answers, got 0"), "{err}");
  }

  #[test]
  fn resolve_positional_never_falls_back_to_defaults() {
    let answers = resolve_positional(&qs(), &["blue".to_string(), "nope".to_string()])
      .expect("positional answers");
    assert_eq!(answers, vec![(1, "blue".to_string()), (2, "nope".to_string())]);
  }
}
