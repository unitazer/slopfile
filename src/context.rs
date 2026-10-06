use crate::session::{self, Session};
use crate::types::{ChatMessage, Role};
use std::path::Path;

const SENTINEL: &str = "Context file:";

pub const CONTEXT_FULL_NOTE: &str =
  "the model hit the token cap mid-response; this session file is your context, so edit it now to \
   drop or compress the useless old turns above, then write your next message below this note";

pub fn context_file_paragraph(path: &Path) -> String {
  format!(
    "\n\n{SENTINEL} {}\n\
     This conversation is that file, and keeping it usable is your job, not the harness's. Edit it \
     with the `edit`/`write` tools or with bash (sed, python, heredocs) to drop dead tool output, \
     compress turns you have finished with, keep a running notes region, or delete whole turns. \
     Nothing is summarized behind your back: whatever you leave in the file is exactly what you \
     get as context on the next call, so prune before the context fills up.\n\
     The file is a stack of regions. `<!-- slopfile:NAME -->` opens one, `<!-- /slopfile -->` \
     closes it, and everything between is the body, copied out verbatim. Keep the ```toml fence and \
     the `system` region; drop or rewrite anything else. A turn counts as finished when its \
     `turn` region is closed, so close the ones you are done with.\n\
     If your own text needs to contain one of those markers, wrap it so it stays text: \
     `<!--slopfile:ESC[<!-- /slopfile -->]-->` keeps a literal close marker inside a body. An edit \
     that would leave the file unparseable is rolled back and the tool result tells you why.",
    path.display()
  )
}

pub fn attach_context_file(messages: &mut [ChatMessage], path: &Path) {
  let Some(system) = messages
    .iter_mut()
    .find(|m| m.role == Role::System && m.content.as_deref().is_some_and(|c| !c.is_empty()))
  else {
    return;
  };
  let content = system.content.as_deref().unwrap_or_default();
  if content.contains(SENTINEL) {
    return;
  }
  system.content = Some(format!("{content}{}", context_file_paragraph(path)));
}

/// Validate a model-authored edit and hand back the session it describes. Returns the reason the
/// edit was refused so the caller can put it in the tool result.
pub fn adopt(edited: &str) -> Result<Session, String> {
  let (parsed, report) = session::parse(edited);
  if !report.is_clean() {
    return Err(report.problems().join("; "));
  }
  if session::fence_bounds(&parsed.preamble).is_none() {
    return Err("the ```toml header must stay at the top of the file".to_string());
  }
  if parsed.system.trim().is_empty() {
    return Err("the `system` region must remain".to_string());
  }
  Ok(parsed)
}

pub fn check_edit(text: &str) -> Result<(), String> {
  adopt(text).map(|_| ())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::session::{escape, ASSISTANT, BEGIN, SYSTEM, TEXT, TURN, USER};

  fn file(body: &str) -> String {
    format!(
      "```toml\nmodel = \"m\"\n```\n<!-- slopfile:{BEGIN} -->\n{body}<!-- slopfile:input -->\n"
    )
  }

  fn good() -> String {
    file(&format!(
      "<!-- slopfile:{SYSTEM} -->\nsys\n<!-- /slopfile -->\n<!-- slopfile:{TURN} -->\n<!-- slopfile:{USER} -->\nhi\n<!-- /slopfile -->\n<!-- /slopfile -->\n"
    ))
  }

  #[test]
  fn paragraph_names_the_path_and_the_region_rules() {
    let p = context_file_paragraph(Path::new("/t/shared/transcripts/1.md"));
    assert!(p.contains("Context file: /t/shared/transcripts/1.md"), "{p}");
    assert!(p.contains("prune before the context fills up"), "{p}");
    assert!(p.contains("<!-- /slopfile -->"), "{p}");
    assert!(p.contains("<!--slopfile:ESC["), "{p}");
    assert!(p.contains("rolled back"), "{p}");
  }

  #[test]
  fn attach_extends_the_system_message_once() {
    let mut msgs = vec![ChatMessage::system("be helpful"), ChatMessage::user("hi")];
    attach_context_file(&mut msgs, Path::new("/t/1.md"));
    let system = msgs[0].content.clone().unwrap();
    assert!(system.starts_with("be helpful"));
    assert!(system.contains("Context file: /t/1.md"), "{system}");
    assert_eq!(msgs[1].content.as_deref(), Some("hi"));

    attach_context_file(&mut msgs, Path::new("/t/1.md"));
    assert_eq!(msgs[0].content.as_deref().unwrap(), system);
  }

  #[test]
  fn attach_is_a_noop_without_a_system_message() {
    let mut msgs = vec![ChatMessage::user("hi")];
    attach_context_file(&mut msgs, Path::new("/t/1.md"));
    assert_eq!(msgs[0].content.as_deref(), Some("hi"));
  }

  #[test]
  fn a_sound_edit_passes() {
    assert!(check_edit(&good()).is_ok());
  }

  #[test]
  fn edits_must_keep_the_toml_fence() {
    let err = check_edit("<!-- slopfile:begin -->\n<!-- slopfile:system -->\ns\n<!-- /slopfile -->\n")
      .expect_err("no fence");
    assert!(err.contains("toml header"), "{err}");
  }

  #[test]
  fn edits_must_keep_the_system_region() {
    let err = check_edit(&file(
      "<!-- slopfile:turn -->\n<!-- /slopfile -->\n",
    ))
    .expect_err("no system region");
    assert!(err.contains("system"), "{err}");
  }

  #[test]
  fn edits_must_keep_the_structure_sound() {
    let err = check_edit(&file("<!-- /slopfile -->\n")).expect_err("stray close");
    assert!(err.contains("never opened"), "{err}");

    let err = check_edit(&file(&format!(
      "<!-- slopfile:{SYSTEM} -->\ns\n<!-- /slopfile -->\n<!-- slopfile:{TURN} -->\n<!-- slopfile:{ASSISTANT} -->\n<!-- slopfile:{TEXT} -->\nleak\n<!-- slopfile:{USER} -->\nnope\n<!-- /slopfile -->\n"
    )))
    .expect_err("unclosed leaf");
    assert!(err.contains("left unclosed"), "{err}");

    let err = check_edit(&file(&format!(
      "<!-- slopfile:{SYSTEM} -->\ns\n<!-- /slopfile -->\n<!-- slopfile:mystery -->\nx\n<!-- /slopfile -->\n"
    )))
    .expect_err("unknown region");
    assert!(err.contains("unknown region"), "{err}");
  }

  #[test]
  fn adopt_takes_a_pruned_edit() {
    let pruned = good().replace("hi\n", "");
    let edited = adopt(&pruned).expect("adopt");
    assert_eq!(edited.turns[0].user.as_deref(), Some(""), "the user text is gone");
    assert_eq!(edited.system, "sys");
  }

  #[test]
  fn adopt_refuses_an_unusable_edit() {
    assert!(adopt("").is_err());
    assert!(adopt("everything is gone\n").is_err());
    let no_system = file(&escape("no system region here\n"));
    assert!(adopt(&no_system).is_err());
  }
}
