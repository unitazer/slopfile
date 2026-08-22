use crate::lex::{lex, TokKind};
use crate::questions::{self, QuestionsBlock};
use crate::tools;
use crate::transcript::{read_file, reassemble, strip_live_line, write_file, ParsedFile};
use crate::types::{AppConfig, Confirmations, LiveStatus, Phase, ToolCall};
use std::path::{Path, PathBuf};

pub fn session_template(cfg: &AppConfig) -> String {
  let mut out = String::new();
  out.push_str("```toml\n");
  out.push_str(&format!("model = \"{}\"\n", cfg.model));
  out.push_str("thinking = \"auto\"\n");
  out.push_str("tools = [\"read\", \"write\", \"edit\", \"bash\", \"grep\", \"find\", \"ls\"]\n");
  out.push_str("```\n");
  out.push_str("# Session\n");
  out.push_str(&system_prompt_default(cfg));
  out.push_str("\n### You\n");
  out
}

fn system_prompt_default(cfg: &AppConfig) -> String {
  let mut out = format!(
    "You are an expert software engineering assistant. You help the user by reading files, running commands, editing code, and writing files, and you reply to them in plain text.

You act by calling tools. Your text replies and tool calls are relayed to the user automatically, one step at a time; continue based on each tool result. When you need the user to make a decision, call the `question` tool and offer concrete, distinct options instead of guessing.

Available tools:
- read: Read files as numbered lines
- write: Create or overwrite files
- edit: Replace the first occurrence of a text fragment in a file
- bash: Execute shell commands (ls, grep, find, etc.) in the workdir
- grep: Search file contents for a substring
- find: Find files by name suffix pattern
- ls: List directory contents

Guidelines:
- Prefer grep/find/ls tools over bash for file exploration
- Use read to examine files before editing. Use this tool instead of cat or sed.
- Use edit for precise changes (old text must match exactly)
- Use write only for new files or complete rewrites
- Be concise in your replies; don't echo back full file contents unless asked
- Show file paths clearly when working with files

Current working directory: {}",
    cfg.workdir
  );
  out.push_str(&skills_index(&cfg.skills_dir));
  out
}

fn skills_index(dir: &str) -> String {
  let entries = match std::fs::read_dir(dir) {
    Ok(entries) => entries,
    Err(_) => return String::new(),
  };
  let mut files: Vec<PathBuf> = entries
    .flatten()
    .map(|e| e.path())
    .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
    .collect();
  files.sort();
  if files.is_empty() {
    return String::new();
  }
  let mut listed = Vec::new();
  for f in files {
    let Ok(content) = std::fs::read_to_string(&f) else {
      continue;
    };
    if frontmatter_disabled(&content) {
      continue;
    }
    let name = f.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    match skill_summary(&content) {
      Some(desc) => listed.push(format!("- {name}: {desc}\n")),
      None => listed.push(format!("- {name}\n")),
    }
  }
  if listed.is_empty() {
    return String::new();
  }
  let mut out = format!(
    "\nSkills available in {dir}. When a task matches a skill, read that file fully and follow it:\n"
  );
  out.push_str(&listed.concat());
  out
}

fn frontmatter_disabled(content: &str) -> bool {
  let mut lines = content.lines();
  if !lines.next().is_some_and(|l| l.trim() == "---") {
    return false;
  }
  for line in lines {
    let trimmed = line.trim();
    if trimmed == "---" || trimmed == "..." {
      break;
    }
    if trimmed.starts_with("disable-model-invocation:") && trimmed.contains("true") {
      return true;
    }
  }
  false
}

fn skill_summary(content: &str) -> Option<String> {
  let lines: Vec<&str> = content.lines().collect();
  if lines.first().is_some_and(|l| l.trim() == "---") {
    return frontmatter_summary(&lines[1..]);
  }
  body_summary(&lines)
}

fn frontmatter_summary(lines: &[&str]) -> Option<String> {
  let mut title: Option<String> = None;
  let mut desc_parts: Vec<String> = Vec::new();
  let mut in_desc = false;
  for line in lines {
    let trimmed = line.trim();
    if trimmed == "---" || trimmed == "..." {
      break;
    }
    if in_desc {
      if line.starts_with(' ') && !trimmed.is_empty() {
        desc_parts.push(trimmed.to_string());
        continue;
      }
      in_desc = false;
    }
    if let Some(rest) = trimmed.strip_prefix("description:") {
      in_desc = true;
      let value = rest
        .trim()
        .trim_start_matches('>')
        .trim_start_matches('|')
        .trim()
        .trim_matches('"')
        .trim_matches('\'');
      if !value.is_empty() {
        desc_parts.push(value.to_string());
      }
    } else if let Some(rest) = trimmed.strip_prefix("name:") {
      title = Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
    }
  }
  let desc = desc_parts.join(" ");
  if desc.is_empty() {
    title.filter(|t| !t.is_empty())
  } else {
    Some(truncate_chars(&desc, 160))
  }
}

fn body_summary(lines: &[&str]) -> Option<String> {
  let mut title: Option<&str> = None;
  let mut desc: Option<String> = None;
  for line in lines {
    let trimmed = line.trim();
    if trimmed.starts_with("```") {
      break;
    }
    if title.is_none() {
      if let Some(rest) = trimmed.strip_prefix("# ") {
        if !rest.is_empty() {
          title = Some(rest.trim());
        }
        continue;
      }
      continue;
    }
    if trimmed.is_empty() || trimmed.starts_with('#') {
      continue;
    }
    desc = Some(truncate_chars(trimmed, 120));
    break;
  }
  desc.or(title.map(|s| s.to_string()))
}

fn truncate_chars(s: &str, max: usize) -> String {
  if s.chars().count() <= max {
    return s.to_string();
  }
  let cut: String = s.chars().take(max).collect();
  match cut.rfind(' ') {
    Some(i) => format!("{}...", &cut[..i]),
    None => format!("{cut}..."),
  }
}

pub fn write_status(path: &Path, pf: &ParsedFile, phase: Phase) -> Result<(), String> {
  let text = read_file(path)?;
  let (body, _live) = strip_live_line(&text);
  let status = LiveStatus {
    phase,
    model: pf.config.model.clone().unwrap_or_default(),
  };
  let json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".into());
  let extra = format!("<!-- slopfile:live {json} -->\n");
  write_file(path, reassemble(&body, &extra, None))
}

pub async fn write_tool_blocks(
  path: &Path,
  calls: &[ToolCall],
  cfg: &AppConfig,
  pf: &ParsedFile,
) -> Result<(), String> {
  let text = read_file(path)?;
  let (body, live) = strip_live_line(&text);

  let mut block = String::new();
  for call in calls {
    let started = std::time::Instant::now();
    let timeout = pf.config.tool_timeout_ms.unwrap_or(cfg.tool_timeout_ms);
    let result = tools::run(
      &call.name,
      &call.arguments,
      PathBuf::from(&cfg.workdir),
      timeout,
      &[PathBuf::from(&cfg.skills_dir)],
    )
    .await;
    let ms = started.elapsed().as_millis();
    block.push_str(&format!("<!-- tool:{} {} -->\n", call.name, call.arguments));
    block.push_str(&result.output);
    if !result.output.ends_with('\n') {
      block.push('\n');
    }
    let status = if result.ok { "ok" } else { "err" };
    block.push_str(&format!("<!-- result:{} {status}={ms}ms -->\n", call.name));
  }

  write_file(path, reassemble(&body, &block, live.as_deref()))
}

pub fn render_questions_block(path: &Path, calls: &[ToolCall]) -> Result<(), String> {
  let call = calls
    .iter()
    .find(|c| c.name == "question")
    .expect("question call");
  let args: serde_json::Value = serde_json::from_str(&call.arguments)
    .map_err(|e| format!("bad question args json: {e}"))?;
  let data = args
    .get("data")
    .and_then(|d| d.as_array())
    .ok_or("question tool needs a data array")?;

  let mut qs = Vec::new();
  let mut stash = Vec::new();
  for (i, item) in data.iter().enumerate() {
    qs.push(questions::question_from_tool_item((i + 1) as u32, item));
    stash.push(item.to_string());
  }

  let text = read_file(path)?;
  let (body, live) = strip_live_line(&text);
  let mut block = questions::render_block(&qs);
  for (q, json) in qs.iter().zip(stash.iter()) {
    block.push_str(&format!("<!-- q:{} {json} -->\n", q.id));
  }
  block.push_str(&format!("<!-- call:{} {} -->\n", call.name, call.arguments));

  write_file(path, reassemble(&body, &block, live.as_deref()))?;
  Ok(())
}

pub fn questions_resolvable(pf: &ParsedFile, cfg: &AppConfig) -> bool {
  if cfg.confirmations == Confirmations::Yolo {
    return true;
  }
  match questions_block_region(&pf.tail) {
    Some(block) => user_answers_from_tail(block).is_some() || has_skip(block),
    None => false,
  }
}

/// End offset of a QUESTIONS block starting at `header_ix`: just past its
/// stashed `<!-- call:... -->` line, or end of text when unstashable.
fn block_end(text: &str, header_ix: usize) -> usize {
  text[header_ix..]
    .find("<!-- call:")
    .and_then(|ix| {
      let line_start = header_ix + ix;
      text[line_start..].find('\n').map(|nl| line_start + nl + 1)
    })
    .unwrap_or(text.len())
}

fn questions_block_region(tail: &str) -> Option<&str> {
  let start = tail.find(questions::BLOCK_HEADER)?;
  Some(&tail[start..block_end(tail, start)])
}

/// The stashed `<!-- call:name {json} -->` payload of a question block.
fn stashed_call(block_text: &str) -> String {
  block_text
    .lines()
    .find_map(|l| {
      let l = l.trim();
      let inner = l.strip_prefix("<!-- call:")?.strip_suffix("-->")?.trim();
      Some(inner.to_string())
    })
    .unwrap_or_else(|| "{}".to_string())
}

fn has_skip(tail: &str) -> bool {
  lex(tail).into_iter().any(|t| t.is_skip())
}

fn user_answers_from_tail(tail: &str) -> Option<String> {
  lex(tail)
    .into_iter()
    .find(|t| t.kind == TokKind::Answer && !t.payload.is_empty() && !t.is_skip())
    .map(|t| t.payload.to_string())
}

pub fn resolve_questions(
  path: &Path,
  text: &str,
  pf: &ParsedFile,
  cfg: &AppConfig,
) -> Result<(), String> {
  let (body, live) = strip_live_line(text);
  let mut out = body.clone();

  for block in &pf.pending_questions {
    let Some(header_ix) = out.find(questions::BLOCK_HEADER) else {
      continue;
    };
    let end = block_end(&out, header_ix);
    let block_text = &out[header_ix..end];

    let skipping = has_skip(block_text);
    let raw = user_answers_from_tail(block_text);
    let yolo = cfg.confirmations == Confirmations::Yolo;
    let answers = match (&raw, skipping) {
      (_, true) => default_answers(block),
      (Some(t), false) => questions::parse_answers(block, t)?,
      (None, false) if yolo => default_answers(block),
      (None, false) => break,
    };

    let call_json = stashed_call(block_text);
    let (name, args) = call_json.split_once(' ').unwrap_or(("question", &call_json));

    let mut tool_out = "\n".to_string();
    tool_out.push_str(&format!("<!-- tool:{name} {args} -->\n"));
    let payload = serde_json::json!({
      "answers": answers.iter().map(|(id, a)| serde_json::json!({"id": id, "answer": a})).collect::<Vec<_>>()
    });
    tool_out.push_str(&payload.to_string());
    tool_out.push('\n');
    tool_out.push_str(&format!("<!-- result:{name} ok=ask -->\n"));

    let mut updated = out.clone();
    updated.replace_range(header_ix..end, &tool_out);
    out = drop_skip_lines(&updated);
  }

  write_file(path, reassemble(&out, "", live.as_deref()))?;
  Ok(())
}

fn default_answers(block: &QuestionsBlock) -> Vec<(u32, String)> {
  block
    .questions
    .iter()
    .map(|q| {
      let ans = q
        .default
        .clone()
        .or_else(|| q.answers.first().cloned())
        .unwrap_or_default();
      (q.id, ans)
    })
    .collect()
}

fn drop_skip_lines(text: &str) -> String {
  let had_nl = text.ends_with('\n');
  let mut out = lex(text)
    .into_iter()
    .filter(|t| !t.is_skip())
    .map(|t| t.text)
    .collect::<Vec<_>>()
    .join("\n");
  if had_nl && !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out
}

pub fn promote_draft(path: &Path, text: &str) -> Result<(), String> {
  let (body, live) = strip_live_line(text);
  let done_ix = crate::transcript::turn_end_tail_start(&body).unwrap_or(0);

  let rest = &body[done_ix..];
  let skip = rest
    .lines()
    .take_while(|l| l.trim().is_empty() || crate::lex::lex_line(l).is_daemon_owned())
    .map(|l| l.len() + 1)
    .sum::<usize>()
    .min(rest.len());

  let marker_end = done_ix + skip;
  let mut out = String::new();
  out.push_str(&body[..marker_end]);
  if !out.is_empty() && !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str("### You\n");
  out.push_str(body[marker_end..].trim_start_matches('\n'));
  if let Some(live) = &live {
    out.push('\n');
    out.push_str(live);
  }
  write_file(path, out)
}
