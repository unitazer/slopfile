use crate::types::{AppConfig, SessionConfig};
use std::path::{Path, PathBuf};

pub fn context_size_note(cfg: &AppConfig, session_config: &SessionConfig) -> String {
  if !cfg.tell_context_size(session_config) {
    return String::new();
  }
  let budget = crate::tokens::context_budget(session_config, cfg);
  let (lo, hi) = crate::tokens::context_bands(budget);
  format!(
    "\n\nContext size. Every tool result you get back is tagged `ctx:N`, the length of this \
     conversation window in tokens as of the request that made that call. It tells you how much \
     room is left so you can prune before you run out, instead of discovering it by being cut off \
     mid-sentence.\n\n \
     If you have only just started, stay under {lo} tokens. If you have a long and detailed \
     context where everything still matters and nothing can be thrown away, try to stay under \
     {hi}.\n\n \
     The number may be stale: it is a snapshot from before the call, and it does not move until \
     the next one. It is a hint about which ballpark you are in, not a live meter -- judge by \
     trend across calls rather than by any single reading."
  )
}

pub fn system_prompt(cfg: &AppConfig, session_config: &SessionConfig, path: &Path) -> String {
  let decision_line = if session_config
    .tools_excluded
    .iter()
    .any(|x| x == "question")
  {
    "You have no way to ask the user anything: nobody will answer a question from you, and no decision is coming. Never block on one. Make the call yourself, and mention any assumption you made at the end -- incorrect assumptions are one of the worst error sources."
  } else {
    "When you need the user to make a decision, call the `question` tool and offer concrete, distinct options instead of guessing."
  };
  let mut out = format!(
    "You are an expert software engineering assistant. You help the user by reading files, running commands, editing code, and writing files, and you reply to them in plain text.

You act by calling tools. Your text replies and tool calls are relayed to the user automatically, one step at a time; continue based on each tool result. {decision_line}
");
  out.push_str(
    "\n\nGuidelines:
- Prefer grep/find/ls tools over bash for file exploration
- Use read to examine files before editing. Use this tool instead of cat or sed.
- Use edit for precise changes (old text must match exactly)
- Use write only for new files or complete rewrites",
  );

  out.push_str(
    "\n- Avoid dumping large file listings into the conversation. Prefer `fd` over find (it respects .gitignore, so build/generated dirs stay out of your context); scope listings to a specific subdirectory and pattern, and pipe through head -50 when output could be long.",
  );
  out.push_str(
    "\n- The container filesystem is ephemeral: anything installed via a package manager vanishes on rebuild. If a required tool or version is missing, stop and tell the user so the image gets updated; do not paper over it with ad-hoc installs.",
  );
  out.push_str(&format!(
    "\n- Be concise in your replies; don't echo back full file contents unless asked
- Show file paths clearly when working with files; use relative paths when full paths get long

Current working directory: {}",
    cfg.workdir
  ));
  out.push_str(&skills_index(&cfg.skills_dir));
  // `skills_index` is empty when there are no skills, and the block above ends without a newline,
  // so this needs its own leading break or it welds itself onto the working directory.
  out.push_str(
    "\n\nIf you learn something that took trial and error to discover, write it down as a new skill. Creating skills is how you manage persistent memory, so the next session does not have to rediscover it."
  );
  out.push_str(&context_size_note(cfg, session_config));
  out.push('\n');
  out.push_str(&context_paragraph(path));
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
    let name = f
      .file_name()
      .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
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
    frontmatter_summary(&lines[1..])
  } else {
    body_summary(&lines)
  }
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
      if let Some(rest) = trimmed.strip_prefix("# ")
        && !rest.is_empty()
      {
        title = Some(rest.trim());
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

/// The paragraph that explains the context file. Written once when the session is created; after
/// that the harness never touches the `system` region again, so the model owns it outright.
fn context_paragraph(path: &Path) -> String {
  format!(
    "Context file: {path}

This conversation is that file. Everything in it is already in your context, so reading it back with \
`read`, `cat`, `grep`, `head`, or `tail` only duplicates it. Do not read your own context file.

Editing it with `edit` does not shrink the window. The edit call is recorded in the same file, so it \
writes back everything it removes. Every such edit grows the file. To compact, use a tool whose \
pattern is smaller than the text it removes: a short path or selector that drops or rewrites a large \
region in one call. Use the `session-edit` skill if available, or write your own tool.

Whatever is left in the file is what you get on the next call, so prune before the context fills up: \
delete dead tool output, compress finished turns, or remove whole turns.

Regions: `<!-- slopfile:NAME -->` opens one, `<!-- /slopfile -->` closes it. The body between them is \
copied verbatim. Keep the ```toml fence and the `system` region; drop or rewrite anything else. A \
turn is finished when its `turn` region is closed.

Grammar: top level takes `system`, `turn`, `input`. A `turn` takes `user`, `note`, `assistant`. An \
`assistant` takes `reasoning`, `text`, `call`. Any other name inside a `turn` or `assistant` is kept \
as a custom role and returned under that name, so you can add a `scratchpad` or `ledger`. `text` \
belongs inside an `assistant`, not directly in a `turn`.

If your text must contain a marker, escape it: `<!--slopfile:ESC[<<!-- /slopfile -->]-->` keeps a \
literal close marker inside a body. An edit that would make the file unparseable is refused, and the \
tool result says why.",
    path = path.display(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn paragraph_names_the_path_and_the_region_rules() {
    let p = context_paragraph(Path::new("/t/shared/transcripts/1.md"));
    assert!(
      p.contains("Context file: /t/shared/transcripts/1.md"),
      "{p}"
    );
    assert!(p.contains("prune before the context fills up"), "{p}");
    assert!(p.contains("<!-- /slopfile -->"), "{p}");
    assert!(p.contains("<!--slopfile:ESC["), "{p}");
    assert!(p.contains("is refused"), "{p}");
  }

  #[test]
  fn system_prompt_names_the_context_file_last() {
    let cfg = AppConfig {
      skills_dir: "/nonexistent".to_string(),
      ..Default::default()
    };
    let path = Path::new("/t/1.md");
    let prompt = system_prompt(&cfg, &SessionConfig::default(), path);
    assert!(
      prompt.contains(&format!("Context file: {}", path.display())),
      "{prompt}"
    );
    assert!(prompt.ends_with("says why."), "{prompt}");
    assert_eq!(prompt.matches("Context file: ").count(), 1, "{prompt}");
  }
}
