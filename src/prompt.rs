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

pub fn system_prompt(cfg: &AppConfig, session_config: &SessionConfig) -> String {
  let has = |t: &str| !session_config.tools_excluded.iter().any(|x| x == t);
  let decision_line = if has("question") {
    "When you need the user to make a decision, call the `question` tool and offer concrete, distinct options instead of guessing."
  } else {
    "You have no way to ask the user anything: nobody will answer a question from you, and no decision is coming. Never block on one. Make the call yourself, and mention any assumption you made at the end -- incorrect assumptions are one of the worst error sources."
  };
  let mut out = format!(
    "You are an expert software engineering assistant. You help the user by reading files, running commands, editing code, and writing files, and you reply to them in plain text.

You act by calling tools. Your text replies and tool calls are relayed to the user automatically, one step at a time; continue based on each tool result. {decision_line}
");
  if has("subagent") {
    out.push_str("\n- subagent: Spawn a worker with its own fresh context to handle a self-contained task; its final report is returned here automatically");
  }
  out.push_str(
    "\n\nGuidelines:
- Prefer grep/find/ls tools over bash for file exploration
- Use read to examine files before editing. Use this tool instead of cat or sed.
- Use edit for precise changes (old text must match exactly)
- Use write only for new files or complete rewrites",
  );
  if has("subagent") {
    out.push_str("\n- Use subagent for large self-contained subtasks (e.g. 'research X and report back', 'fix all lint errors in module Y'); give it every detail it needs, since it cannot see this conversation. Spawn it, then wait for its report instead of duplicating its work.");
  }

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

/// Append the paragraph that explains the context file. Called once when the session is created;
/// after that the harness never touches the `system` region again, so the model owns it outright.
pub fn with_context_paragraph(system: &str, path: &Path) -> String {
  let base = system.trim_end();
  let paragraph = context_paragraph(path);
  if base.is_empty() {
    paragraph
  } else {
    format!("{base}\n\n{paragraph}")
  }
}

fn context_paragraph(path: &Path) -> String {
  format!(
    "Context file: {path}
This conversation is that file, and every byte of it is already in your context, so reading it back \
(`read`, `cat`, `grep`, `head`, `tail`) only re-injects what you can already see: a gigantic waste of \
tokens. Never read your own context file.

Editing it directly is just as useless. `edit` needs an old string and a new string, and the call \
that carries them is itself recorded in this very file, so it writes back everything it removes. \
Every such edit is a net gain of tokens, never a loss, and the window can never shrink through one: \
editing your context window is impossible. All compaction must go through a tool whose pattern is \
smaller than the chunk it removes: a short path or selector that drops or rewrites a big region in \
a few dozen bytes of call. If there is a tool to edit your context window already available (eg the \
`session-edit` skill), use that; if not, invent your own tooling.

Whatever you leave in the file is exactly what you get as context on the next call, so prune before \
the context fills up: drop dead tool output, compress turns you have finished with, or delete whole \
turns.

The file is a stack of regions. `<!-- slopfile:NAME -->` opens one, `<!-- /slopfile -->` closes it, \
and everything between is the body, copied out verbatim. Keep the ```toml fence and the `system` \
region; drop or rewrite anything else. A turn counts as finished when its `turn` region is closed, \
so close the ones you are done with.

The grammar is: top level takes `system`, `turn` and `input`; a `turn` takes `user`, `note`, \
`assistant`; an `assistant` takes `reasoning`, `text` and `call`. Anything else you name inside a \
`turn` or an `assistant` is kept as your own role and handed back to you under that name, so you \
can invent a `scratchpad` or a `ledger` and write in it as long as you like. `text` goes inside an \
`assistant`, never straight into a `turn`.

If your own text needs to contain one of those markers, wrap it so it stays text: \
`<!--slopfile:ESC[<<!-- /slopfile -->]-->` keeps a literal close marker inside a body. An edit that \
would leave the file unparseable is rolled back and the tool result tells you why.",
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
    assert!(p.contains("rolled back"), "{p}");
  }

  #[test]
  fn with_context_paragraph_appends_it_to_the_default_prompt() {
    let system = with_context_paragraph("be helpful", Path::new("/t/1.md"));
    assert!(system.starts_with("be helpful"), "{system}");
    assert_eq!(system.matches("Context file: ").count(), 1, "{system}");
  }
}
