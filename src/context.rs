use crate::session::{self, Session};
use crate::types::{ChatMessage, Role};
use std::path::Path;

/// The output allowance ran out because the model spent it thinking, not because the transcript
/// grew. The advice is therefore the opposite of `CONTEXT_FULL_NOTE`: leave the file alone.
pub const REASONING_EXHAUSTED_NOTE: &str = "the model used its entire output allowance on \
   thinking and wrote nothing back; nothing is wrong with this transcript, so do not prune it. \
   Lower `thinking` in the fence above, or raise `max_context_tokens` so the allowance is bigger";

pub const CONTEXT_FULL_NOTE: &str = "the model hit the token cap mid-response; this session file is your context, so edit it now to \
   drop or compress the useless old turns above, then write your next message below this note";

/// Plain-text fences, not region markers: this text is written into the `system` region, and any
/// `<` in a body gets escaped on render. Fences made of `<` would litter the file with escape
/// wrappers, so the managed block is delimited by something a human can read and a parser can
/// strip deterministically.
pub const CONTEXT_BEGIN: &str = "--- context (managed by slopfile) ---";
pub const CONTEXT_END: &str = "--- end context ---";

/// Fill level at which the model is told to prune. Below this the paragraph stays quiet: nagging
/// about headroom it does not need trains the model to ignore the line, and the paper's whole point
/// is that compaction timing should be the model's call, not a reflex triggered by noise.
pub const PRUNE_THRESHOLD_PCT: u32 = 70;

#[derive(Clone, Copy, Debug)]
pub struct Window {
  pub used: usize,
  /// Tokens the prompt may occupy: the window minus what is reserved for the answer. This is the
  /// denominator, not `total`, and the difference is the whole ballgame -- a reply has to fit
  /// too, so the prompt is held below `total` and never reaches it. Measured against the full
  /// window the percentage tops out under the threshold and the reminder never fires at all: at a
  /// 65k window it could not exceed 38%.
  pub usable: u32,
  /// The full context window, which the model still needs to reason about.
  pub total: u32,
}

impl Window {
  fn percent(&self) -> u32 {
    if self.usable == 0 {
      return 0;
    }
    (self.used as u64 * 100 / u64::from(self.usable)) as u32
  }
}

/// The one line that gives the model a reason to act. It is asked to prune "before the context
/// fills up" but has no way to know how full it is, and the only other signal it gets arrives
/// mid-response once the answer is already truncated.
fn pressure_line(window: Option<Window>) -> String {
  let Some(w) = window else {
    return String::new();
  };
  let pct = w.percent();
  if pct < PRUNE_THRESHOLD_PCT {
    return String::new();
  }
  format!(
    "\nWindow: {} tokens, of which {} are available for this conversation; about {} used ({}%). \
     Prune before you continue: drop dead tool \
     output, compress turns you have finished with, and delete anything you have already used. \
     Whatever you leave in this file is what you get back, so make room for the work ahead.\n",
    w.total, w.usable, w.used, pct
  )
}

pub fn context_paragraph_with(path: &Path, window: Option<Window>) -> String {
  format!(
    "{CONTEXT_BEGIN}
Context file: {path}{pressure}

This conversation is that file, and keeping it usable is your job, not the harness's. Edit it with \
the `edit`/`write` tools or with bash (sed, python, heredocs) to drop dead tool output, compress \
turns you have finished with, keep a running `note` region, or invent a role of your own for it, or delete whole turns. Nothing is \
summarized behind your back: whatever you leave in the file is exactly what you get as context on \
the next call, so prune before the context fills up.

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
would leave the file unparseable is rolled back and the tool result tells you why.
{CONTEXT_END}",
    path = path.display(),
    pressure = pressure_line(window),
  )
}

/// Drop any previously written managed block and append a freshly rendered one. Called on every
/// request so the path in the file is always the path we are actually serving, even if the
/// transcript was moved, and called once at creation so the file explains itself to a human too.
pub fn refresh_paragraph(system: &str, path: &Path, window: Option<Window>) -> String {
  let mut kept = String::with_capacity(system.len());
  let mut rest = system;
  while let Some(start) = rest.find(CONTEXT_BEGIN) {
    kept.push_str(&rest[..start]);
    rest = &rest[start..];
    match rest.find(CONTEXT_END) {
      // Drop through the end fence and the newline that followed it.
      Some(end) => rest = rest[end + CONTEXT_END.len()..].trim_start_matches('\n'),
      // A truncated block (the model was mid-edit) still counts as ours to replace.
      None => {
        rest = "";
        break;
      }
    }
  }
  kept.push_str(rest);
  let base = kept.trim_end();
  let block = context_paragraph_with(path, window);
  if base.is_empty() {
    block
  } else {
    format!("{base}\n\n{block}")
  }
}

pub fn attach_context_file(messages: &mut [ChatMessage], path: &Path, window: Option<Window>) {
  let Some(system) = messages
    .iter_mut()
    .find(|m| m.role == Role::System && m.content.as_deref().is_some_and(|c| !c.is_empty()))
  else {
    return;
  };
  let current = system.content.as_deref().unwrap_or_default().to_string();
  system.content = Some(refresh_paragraph(&current, path, window));
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
  use crate::session::{ASSISTANT, BEGIN, SYSTEM, TEXT, TURN, USER, escape};

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
    let p = context_paragraph_with(Path::new("/t/shared/transcripts/1.md"), None);
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
  fn refreshing_replaces_the_block_instead_of_stacking_it() {
    let path = Path::new("/t/1.md");
    let once = refresh_paragraph("be helpful", path, None);
    let twice = refresh_paragraph(&once, path, None);
    assert_eq!(once, twice, "the managed block must be idempotent");
    assert_eq!(
      twice.matches(CONTEXT_BEGIN).count(),
      1,
      "the block was stacked: {twice}"
    );
  }

  #[test]
  fn refreshing_repoints_a_moved_transcript_at_its_new_path() {
    let first = refresh_paragraph("base", Path::new("/old/1.md"), None);
    let moved = refresh_paragraph(&first, Path::new("/new/1.md"), None);
    assert!(moved.contains("/new/1.md"), "{moved}");
    assert!(
      !moved.contains("/old/1.md"),
      "a stale path survived: {moved}"
    );
    assert!(
      moved.starts_with("base"),
      "the model-authored part was lost: {moved}"
    );
  }

  #[test]
  fn refreshing_recovers_from_a_truncated_block() {
    let half = format!("base\n{}\nsome text the model was writing", CONTEXT_BEGIN);
    let fixed = refresh_paragraph(&half, Path::new("/t/1.md"), None);
    assert_eq!(fixed.matches(CONTEXT_BEGIN).count(), 1, "{fixed}");
    assert!(
      !fixed.contains("some text the model was writing"),
      "{fixed}"
    );
  }

  #[test]
  fn attach_extends_the_system_message_once() {
    let mut msgs = vec![ChatMessage::system("be helpful"), ChatMessage::user("hi")];
    attach_context_file(&mut msgs, Path::new("/t/1.md"), None);
    let system = msgs[0].content.clone().unwrap();
    assert!(system.starts_with("be helpful"));
    assert!(system.contains("Context file: /t/1.md"), "{system}");
    assert_eq!(msgs[1].content.as_deref(), Some("hi"));

    attach_context_file(&mut msgs, Path::new("/t/1.md"), None);
    assert_eq!(msgs[0].content.as_deref().unwrap(), system);
  }

  #[test]
  fn attach_is_a_noop_without_a_system_message() {
    let mut msgs = vec![ChatMessage::user("hi")];
    attach_context_file(&mut msgs, Path::new("/t/1.md"), None);
    assert_eq!(msgs[0].content.as_deref(), Some("hi"));
  }

  #[test]
  fn a_sound_edit_passes() {
    assert!(check_edit(&good()).is_ok());
  }

  #[test]
  fn edits_must_keep_the_toml_fence() {
    let err =
      check_edit("<!-- slopfile:begin -->\n<!-- slopfile:system -->\ns\n<!-- /slopfile -->\n")
        .expect_err("no fence");
    assert!(err.contains("toml header"), "{err}");
  }

  #[test]
  fn edits_must_keep_the_system_region() {
    let err = check_edit(&file("<!-- slopfile:turn -->\n<!-- /slopfile -->\n"))
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
    let no_system = file(&escape("no system region here\n"));
    assert!(adopt(&no_system).is_err());
  }
}
