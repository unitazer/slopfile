//! QUESTIONS blocks: rendered in the session tail when the model calls the
//! `question` tool. The user answers below the `>` marker; the daemon parses
//! the answers (1-based indices into the answers array, or verbatim text,
//! comma-separated) and turns them into a normal tool result for the model.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub id: u32,
    pub question: String,
    pub answers: Vec<String>,
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionsBlock {
    pub questions: Vec<Question>,
    /// Raw JSON stash comments (`<!-- q:N {...} -->`) in question order.
    pub raw_json: Vec<String>,
}

pub const BLOCK_HEADER: &str = "❓ QUESTIONS";

/// Render the QUESTIONS block (markdown table + answer instructions).
pub fn render_block(qs: &[Question]) -> String {
    let mut out = String::new();
    out.push_str(BLOCK_HEADER);
    out.push('\n');
    out.push_str("| # | Question | Options |\n");
    out.push_str("|---|----------|---------|\n");
    for q in qs {
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            q.id,
            q.question.replace('|', "\\|"),
            q.answers.join(", ")
        ));
    }
    out.push_str("ANSWER: (numbers or text, comma separated)\n");
    out.push_str("> \n");
    out
}

/// Find and parse a QUESTIONS block anywhere in `text`.
pub fn parse_block(text: &str) -> Option<QuestionsBlock> {
    let start = text.find(BLOCK_HEADER)?;
    let rest = &text[start..];

    let mut questions: Vec<Question> = Vec::new();
    let mut raw_json: Vec<String> = Vec::new();

    // Stash comments appear after the header, one per question:
    // `<!-- q:1 {...} -->`. Build a lookup by id.
    let stashes: Vec<(u32, String)> = rest
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let inner = line.strip_prefix("<!--")?.strip_suffix("-->")?.trim();
            let inner = inner.strip_prefix("q:")?;
            let (id_str, json) = inner.split_once(' ')?;
            Some((id_str.trim().parse::<u32>().ok()?, json.trim().to_string()))
        })
        .collect();

    let stash_by_id: std::collections::HashMap<u32, (String, Option<Question>)> = stashes
        .into_iter()
        .map(|(id, json)| {
            let q = serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| {
                    let question = v.get("question")?.as_str()?.to_string();
                    let answers = v
                        .get("answers")?
                        .as_array()?
                        .iter()
                        .map(|a| a.as_str().unwrap_or("").to_string())
                        .collect::<Vec<_>>();
                    let default = v.get("default").and_then(|d| d.as_str()).map(String::from);
                    Some(Question {
                        id,
                        question,
                        answers,
                        default,
                    })
                });
            (id, (json, q))
        })
        .collect();

    // Parse table rows: `| 1 | red or blue | red, blue |`
    let mut found_header_row = false;
    for line in rest.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Stop at the ANSWER: line.
        if line.starts_with("ANSWER") {
            break;
        }
        if line.starts_with("|---") || line.starts_with("| #") {
            continue;
        }
        let Some(cells) = parse_table_row(line) else {
            continue;
        };
        let id = match cells.first().and_then(|c| c.trim().parse::<u32>().ok()) {
            Some(id) => id,
            None => continue,
        };
        let Some(question) = cells.get(1).map(|c| c.trim().to_string()) else {
            continue;
        };
        let answers = cells
            .get(2)
            .map(|c| c.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default();

        found_header_row = true;

        if let Some((json, Some(q))) = stash_by_id.get(&id) {
            raw_json.push(json.clone());
            questions.push(q.clone());
        } else {
            raw_json.push(String::new());
            questions.push(Question {
                id,
                question,
                answers,
                default: None,
            });
        }
    }

    if !found_header_row && questions.is_empty() {
        return None;
    }

    Some(QuestionsBlock { questions, raw_json })
}

fn parse_table_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !(trimmed.starts_with('|') && trimmed.ends_with('|')) {
        return None;
    }
    let inner = trimmed.trim_matches('|');
    Some(
        inner
            .split('|')
            .map(|c| c.trim().to_string())
            .collect::<Vec<_>>(),
    )
}

/// Parse the user's answer text below the `>` marker.
///
/// Tokens are comma- and newline-separated. A token that parses as a
/// 1-based integer selects `answers[k-1]`; anything else is used verbatim
/// (trimmed). Every question in the block must be answered, else `Err`.
pub fn parse_answers(block: &QuestionsBlock, answer_text: &str) -> Result<Vec<(u32, String)>, String> {
    let mut tokens: VecDeque<String> = answer_text
        .split([',', '\n'])
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect();

    let mut out = Vec::new();
    for q in &block.questions {
        let token = tokens
            .pop_front()
            .or_else(|| q.default.clone())
            .ok_or_else(|| format!("missing answer for question(s): {}", q.id))?;

        let resolved = resolve_token(&token, q);
        out.push((q.id, resolved));
    }

    if out.len() != block.questions.len() {
        let missing: Vec<u32> = block
            .questions
            .iter()
            .map(|q| q.id)
            .filter(|id| !out.iter().any(|(qid, _)| qid == id))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "missing answer for question(s): {}",
                missing
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    Ok(out)
}

fn resolve_token(token: &str, q: &Question) -> String {
    // A token may itself be comma-separated (e.g. multi-select) — not
    // supported per-question; treat the token as one answer.
    if let Ok(k) = token.parse::<usize>() {
        if k >= 1 && k <= q.answers.len() {
            return q.answers[k - 1].clone();
        }
    }
    token.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_block() -> QuestionsBlock {
        QuestionsBlock {
            questions: vec![
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
            ],
            raw_json: vec![
                r#"{"question":"red or blue","answers":["red","blue"]}"#.to_string(),
                r#"{"question":"how long?","answers":["3","5","10"],"default":"5"}"#.to_string(),
            ],
        }
    }

    #[test]
    fn render_then_parse_round_trips() {
        let block = sample_block();
        let rendered = render_block(&block.questions);
        assert!(rendered.contains("❓ QUESTIONS"));
        assert!(rendered.contains("| 1 | red or blue | red, blue |"));
        assert!(rendered.contains("| 2 | how long? | 3, 5, 10 |"));
        assert!(rendered.ends_with("> \n"));

        let reparsed = parse_block(&rendered).expect("must parse");
        assert_eq!(reparsed.questions.len(), 2);
        assert_eq!(reparsed.questions[0].question, "red or blue");
        assert_eq!(reparsed.questions[0].answers, vec!["red", "blue"]);
        assert_eq!(reparsed.questions[1].answers, vec!["3", "5", "10"]);
    }

    #[test]
    fn parse_block_with_stash_comments_prefers_stash() {
        let md = format!(
            "{}\n| # | Question | Options |\n| 1 | red or blue | red, blue |\nANSWER: (numbers or text, comma separated)\n> \n<!-- q:1 {{\"question\":\"red or blue\",\"answers\":[\"red\",\"blue\"],\"default\":\"blue\"}} -->",
            BLOCK_HEADER
        );
        let block = parse_block(&md).expect("parse");
        assert_eq!(block.questions[0].default.as_deref(), Some("blue"));
        assert_eq!(block.raw_json[0], "{\"question\":\"red or blue\",\"answers\":[\"red\",\"blue\"],\"default\":\"blue\"}");
    }

    #[test]
    fn parse_answers_uses_indices() {
        let block = sample_block();
        let answers = parse_answers(&block, "1\n2").expect("answers");
        assert_eq!(answers, vec![(1, "red".to_string()), (2, "5".to_string())]);
    }

    #[test]
    fn parse_answers_uses_verbatim_text() {
        let block = sample_block();
        let answers = parse_answers(&block, "purple\n4").expect("answers");
        assert_eq!(answers, vec![(1, "purple".to_string()), (2, "4".to_string())]);
    }

    #[test]
    fn parse_answers_mixed() {
        let block = sample_block();
        let answers = parse_answers(&block, "2, 3").expect("answers");
        assert_eq!(answers, vec![(1, "blue".to_string()), (2, "10".to_string())]);
    }

    #[test]
    fn parse_answers_uses_default_when_short() {
        let block = sample_block();
        let answers = parse_answers(&block, "1").expect("default fallback");
        assert_eq!(answers, vec![(1, "red".to_string()), (2, "5".to_string())]);
    }

    #[test]
    fn parse_answers_reports_missing() {
        let block = QuestionsBlock {
            questions: vec![
                Question {
                    id: 1,
                    question: "a?".to_string(),
                    answers: vec!["x".into(), "y".into()],
                    default: None,
                },
                Question {
                    id: 2,
                    question: "b?".to_string(),
                    answers: vec!["u".into(), "v".into()],
                    default: None,
                },
            ],
            raw_json: vec![],
        };
        let err = parse_answers(&block, "1").expect_err("must fail");
        assert!(err.contains("missing answer for question(s)"), "{err}");
    }

    #[test]
    fn parse_answers_out_of_range_index_uses_verbatim() {
        let block = sample_block();
        let answers = parse_answers(&block, "9\nx").expect("fallback");
        assert_eq!(answers[0].1, "9");
    }

    #[test]
    fn parse_block_not_found() {
        assert!(parse_block("no questions here").is_none());
    }
}