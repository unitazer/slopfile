
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{error, info, warn};

pub const BASELINE_TAG: &str = "research-best";
const JOURNAL_FILE: &str = "JOURNAL.md";
const AGENTS_FILE: &str = "AGENTS.md";
const JOURNAL_SEED_CAP: usize = 12_000;
const EVAL_OUTPUT_CAP: usize = 4_000;

#[derive(Debug, Clone)]
pub struct ResearchOpts {
  pub workdir: PathBuf,
  pub transcripts_dir: PathBuf,
  pub model: String,
  pub eval_cmd: String,
  pub maximize: bool,
  pub max_iters: u32,
  pub eval_timeout_secs: u64,
  pub poll_ms: u64,
}

impl Default for ResearchOpts {
  fn default() -> Self {
    Self {
      workdir: PathBuf::from("."),
      transcripts_dir: PathBuf::from("transcripts"),
      model: String::new(),
      eval_cmd: "cargo run --release --bin eval".to_string(),
      maximize: false,
      max_iters: 100,
      eval_timeout_secs: 600,
      poll_ms: 2_000,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IterationResult {
  pub solved: bool,
  pub aborted: bool,
}


pub async fn run(opts: ResearchOpts) -> Result<(), String> {
  verify_workdir(&opts)?;
  let agents_md =
    std::fs::read_to_string(opts.workdir.join(AGENTS_FILE)).map_err(|e| format!("{AGENTS_FILE}: {e}"))?;
  std::fs::create_dir_all(&opts.transcripts_dir).map_err(|e| format!("transcripts dir: {e}"))?;

  let mut best = read_best_score(&opts.workdir)?;
  let mut iter = next_iter_number(&opts.transcripts_dir);
  info!(
    "research: workdir={} eval=\"{}\" direction={} best={:?} starting at iter {iter}",
    opts.workdir.display(),
    opts.eval_cmd,
    if opts.maximize { "maximize" } else { "minimize" },
    best
  );

  loop {
    if iter > opts.max_iters {
      info!("research: iteration budget of {} reached; stopping", opts.max_iters);
      break;
    }

    sanitize(&opts.workdir)?;

    let journal = std::fs::read_to_string(opts.workdir.join(JOURNAL_FILE)).unwrap_or_default();
    let path = seed_transcript(&opts, &agents_md, &journal, best, iter)?;
    info!("→ seeded {} (iter {iter})", path.display());

    let outcome = wait_for_iteration(&path, opts.poll_ms).await?;
    if outcome.aborted {
      warn!("✗ iter {iter}: session was aborted");
    }
    if outcome.solved {
      info!("★ iter {iter}: model declared the problem solved");
    }

    let eval = run_eval(&opts).await;

    let mut kept = false;
    if let Ok(score) = eval
      && is_better(score, best, opts.maximize)
    {
      match commit_baseline(&opts.workdir, iter, score) {
        Ok(()) => {
          best = Some(score);
          kept = true;
          info!("✓ iter {iter}: new best score {score}");
        }
        Err(e) => {
          error!("iter {iter}: commit failed: {e}");
        }
      }
    }
    if !kept {
      sanitize(&opts.workdir)?;
    }

    append_log(
      &opts.transcripts_dir,
      iter,
      eval.as_ref().ok().copied(),
      best,
      kept,
      outcome.solved,
      eval.as_ref().err().cloned(),
    )?;

    if outcome.solved {
      info!("research: campaign complete at iteration {}", iter.saturating_sub(1));
      break;
    }
    iter += 1;
  }
  Ok(())
}

fn verify_workdir(opts: &ResearchOpts) -> Result<(), String> {
  let wd = &opts.workdir;
  if !wd.is_dir() {
    return Err(format!("workdir does not exist: {}", wd.display()));
  }
  git(wd, &["rev-parse", "--git-dir"]).map_err(|_| format!("{} is not a git repository", wd.display()))?;
  if !wd.join(AGENTS_FILE).is_file() {
    return Err(format!("missing {AGENTS_FILE} in {}", wd.display()));
  }
  Ok(())
}

pub fn is_better(score: f64, best: Option<f64>, maximize: bool) -> bool {
  match best {
    None => true,
    Some(b) => {
      if maximize {
        score > b
      } else {
        score < b
      }
    }
  }
}


fn git(workdir: &Path, args: &[&str]) -> Result<String, String> {
  let out = std::process::Command::new("git")
    .args(args)
    .current_dir(workdir)
    .stdin(Stdio::null())
    .output()
    .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
  if !out.status.success() {
    let mut msg = format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
      msg.push('\n');
      msg.push_str(stdout.trim());
    }
    return Err(msg);
  }
  Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn baseline_exists(workdir: &Path) -> bool {
  git(workdir, &["rev-parse", "-q", "--verify", &format!("refs/tags/{BASELINE_TAG}")]).is_ok()
}

pub fn sanitize(workdir: &Path) -> Result<(), String> {
  let has_commits = git(workdir, &["rev-parse", "-q", "--verify", "HEAD"]).is_ok();
  if has_commits {
    let target = if baseline_exists(workdir) { BASELINE_TAG } else { "HEAD" };
    git(workdir, &["reset", "--hard", target])?;
  }
  git(workdir, &["clean", "-fd"])?;
  Ok(())
}

pub fn commit_baseline(workdir: &Path, iter: u32, score: f64) -> Result<(), String> {
  git(workdir, &["add", "-A"])?;
  let msg = format!("research: iter {iter} score={score}");
  if let Err(e) = git(workdir, &["commit", "-m", &msg]) {
    if e.contains("nothing to commit") {
      git(workdir, &["commit", "--allow-empty", "-m", &msg])?;
    } else {
      return Err(e);
    }
  }
  git(workdir, &["tag", "-f", BASELINE_TAG])?;
  Ok(())
}

pub fn read_best_score(workdir: &Path) -> Result<Option<f64>, String> {
  if !baseline_exists(workdir) {
    return Ok(None);
  }
  let subject = git(workdir, &["log", "-1", "--format=%s", &format!("refs/tags/{BASELINE_TAG}")])?;
  Ok(parse_score_subject(&subject))
}

pub fn parse_score_subject(subject: &str) -> Option<f64> {
  subject
    .split_whitespace()
    .find_map(|t| t.strip_prefix("score=").and_then(|v| v.parse::<f64>().ok()))
}


pub async fn run_eval(opts: &ResearchOpts) -> Result<f64, String> {
  let mut child = Command::new("sh")
    .arg("-c")
    .arg(&opts.eval_cmd)
    .current_dir(&opts.workdir)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .process_group(0)
    .spawn()
    .map_err(|e| format!("eval: failed to spawn sh: {e}"))?;
  let mut stdout = child.stdout.take().expect("stdout is piped");
  let mut stderr = child.stderr.take().expect("stderr is piped");

  let timeout_secs = if opts.eval_timeout_secs == 0 { u64::MAX } else { opts.eval_timeout_secs };
  let outcome = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
    let (mut out_s, mut err_s) = (String::new(), String::new());
    let _ = stdout.read_to_string(&mut out_s).await;
    let _ = stderr.read_to_string(&mut err_s).await;
    let status = child
      .wait()
      .await
      .map_err(|e| format!("eval: wait failed: {e}"))?;
    Ok::<_, String>((out_s, err_s, status))
  })
  .await;

  match outcome {
    Err(_) => {
      let group_id = child.id();
      let _ = child.kill().await;
      if let Some(id) = group_id {
        let _ = std::process::Command::new("kill")
          .args(["-9", &format!("-{id}")])
          .stderr(Stdio::null())
          .status();
      }
      Err(format!("eval: timed out after {timeout_secs}s: {}", opts.eval_cmd))
    }
    Ok(Err(e)) => Err(e),
    Ok(Ok((out_s, err_s, status))) => {
      if !status.success() {
        let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
        return Err(format!("eval: exit {code}:\n{}", truncate_chars(&err_s, EVAL_OUTPUT_CAP)));
      }
      parse_eval_output(&out_s)
        .ok_or_else(|| format!("eval: no number found in stdout:\n{}", truncate_chars(&out_s, EVAL_OUTPUT_CAP)))
    }
  }
}

pub fn parse_eval_output(stdout: &str) -> Option<f64> {
  let trimmed = stdout.trim();
  if let Ok(v) = trimmed.parse::<f64>() {
    return Some(v);
  }
  stdout
    .lines()
    .rev()
    .map(str::trim)
    .find(|l| !l.is_empty())
    .and_then(|l| l.parse::<f64>().ok())
}


const RESEARCH_SYSTEM_PROMPT: &str = "\
You are an autonomous research agent running unattended iterations on a measurable task.
You act through tools only; there is no `question` tool and nobody will answer. Make reasonable decisions yourself.
This session is exactly ONE iteration of the campaign: explore a single direction, measure it, update the journal, then stop.
The harness re-measures your result itself afterwards and commits or restores the workspace; you must not run git commit/restore/stash.";

pub fn seed_prompt(iter: u32, agents_md: &str, journal: &str, best: Option<f64>, maximize: bool, eval_cmd: &str) -> String {
  let direction = if maximize { "higher" } else { "lower" };
  let score_line = match best {
    Some(b) => format!("Best measured score so far: {b}. {direction} is better."),
    None => "No baseline exists yet: there may be no working implementation at all. \
       Your first job is to produce something the eval command can run against; \
       any successful measurement becomes the baseline."
      .to_string(),
  };
  let journal_block = if journal.trim().is_empty() {
    "(JOURNAL.md does not exist yet — create it this iteration)".to_string()
  } else {
    truncate_head(journal, JOURNAL_SEED_CAP)
  };

  format!(
    "This is iteration {iter} of an autonomous optimization campaign.\n\
\n\
## Task\n\
--- BEGIN AGENTS.md ---\n\
{agents_md}\n\
--- END AGENTS.md ---\n\
\n\
## Score\n\
{score_line}\n\
The eval command is: `{eval_cmd}`\n\
Never modify the evaluation code to inflate the score — that is cheating; the harness re-runs the eval itself.\n\
\n\
## Journal\n\
JOURNAL.md is your persistent memory across iterations. It already contains progress notes,\n\
promising directions and failed ideas. Consult it so you never repeat a known-dead end.\n\
--- BEGIN JOURNAL.md ---\n\
{journal_block}\n\
--- END JOURNAL.md ---\n\
\n\
## Your job this iteration\n\
1. Pick exactly ONE promising improvement direction (check JOURNAL.md first).\n\
2. Implement that single change.\n\
3. Sanity-check by running `{eval_cmd}` yourself.\n\
4. Append to JOURNAL.md: what you tried, the measured result, whether it worked, and ideas worth trying next.\n\
5. Stop with a short summary of what you did and the measured number.\n\
\n\
Rules:\n\
- Do NOT run git commit, restore, stash or reset — the harness owns version control.\n\
- One direction per iteration; do not stack unrelated changes.\n\
- If, and only if, nothing worthwhile remains to explore and the problem counts as solved\n\
  according to AGENTS.md, call the `solve` tool with a summary instead of starting another direction.",
    iter = iter,
    agents_md = agents_md.trim_end(),
    score_line = score_line,
    eval_cmd = eval_cmd,
    journal_block = journal_block.trim_end(),
  )
}

pub fn research_session_template(model: &str, seed_prompt: &str) -> String {
  let mut tools = crate::flow::DEFAULT_TOOLS.to_vec();
  tools.retain(|t| *t != "subagent");
  tools.push("solve");
  format!(
    "{}# Session\n{RESEARCH_SYSTEM_PROMPT}\n\n### You\n{seed_prompt}\n{}\n",
    crate::flow::session_header(model, &tools, &["question".to_string()]),
    crate::transcript::SEND_MARKER
  )
}

pub fn seed_transcript(
  opts: &ResearchOpts,
  agents_md: &str,
  journal: &str,
  best: Option<f64>,
  iter: u32,
) -> Result<PathBuf, String> {
  let prompt = seed_prompt(iter, agents_md, journal, best, opts.maximize, &opts.eval_cmd);
  let path = opts.transcripts_dir.join(format!("research-{iter:03}.md"));
  std::fs::write(&path, research_session_template(&opts.model, &prompt))
    .map_err(|e| format!("write {}: {e}", path.display()))?;
  Ok(path)
}

pub fn next_iter_number(transcripts_dir: &Path) -> u32 {
  let mut max = 0u32;
  if let Ok(entries) = std::fs::read_dir(transcripts_dir) {
    for entry in entries.flatten() {
      let name = entry.file_name();
      let name = name.to_string_lossy();
      let Some(rest) = name.strip_prefix("research-") else {
        continue;
      };
      let Some(digits) = rest.strip_suffix(".md") else {
        continue;
      };
      if let Ok(n) = digits.parse::<u32>() {
        max = max.max(n);
      }
    }
  }
  max + 1
}

async fn wait_for_iteration(path: &Path, poll_ms: u64) -> Result<IterationResult, String> {
  loop {
    tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    if text.contains(crate::lex::SOLVED_MARKER) {
      return Ok(IterationResult { solved: true, aborted: false });
    }
    if text.contains(crate::lex::ABORTED_MARKER) {
      return Ok(IterationResult { solved: false, aborted: true });
    }
    if text.contains(crate::lex::CONTEXT_FULL_PREFIX) {
      warn!(
        "{}: model exhausted its context mid-iteration; discarding this attempt",
        path.display()
      );
      return Ok(IterationResult { solved: false, aborted: false });
    }
    if iteration_finished(&text) {
      return Ok(IterationResult { solved: false, aborted: false });
    }
  }
}

pub fn iteration_finished(text: &str) -> bool {
  crate::transcript::turn_cycle_complete(text)
}


#[derive(Serialize)]
struct LogLine<'a> {
  ts: u64,
  iter: u32,
  score: Option<f64>,
  best: Option<f64>,
  kept: bool,
  solved: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<&'a str>,
}

fn append_log(
  dir: &Path,
  iter: u32,
  score: Option<f64>,
  best: Option<f64>,
  kept: bool,
  solved: bool,
  error: Option<String>,
) -> Result<(), String> {
  let line = LogLine {
    ts: std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_secs(),
    iter,
    score,
    best,
    kept,
    solved,
    error: error.as_deref(),
  };
  let json = serde_json::to_string(&line).map_err(|e| format!("log serialize: {e}"))?;
  let path = dir.join("research-log.jsonl");
  let mut f = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(&path)
    .map_err(|e| format!("open {}: {e}", path.display()))?;
  writeln!(f, "{json}").map_err(|e| format!("append {}: {e}", path.display()))
}

fn truncate_chars(s: &str, max: usize) -> String {
  if s.chars().count() <= max {
    return s.to_string();
  }
  let cut: String = s.chars().take(max).collect();
  format!("{cut}\n… [truncated]")
}

fn truncate_head(s: &str, max_chars: usize) -> String {
  let count = s.chars().count();
  if count <= max_chars {
    return s.to_string();
  }
  let skip = count - max_chars;
  let tail: String = s.chars().skip(skip).collect();
  let tail = tail[tail.find('\n').map(|i| i + 1).unwrap_or(0)..].to_string();
  format!("… [earlier journal entries truncated]\n{tail}")
}


#[cfg(test)]
mod tests {
  use super::*;

  fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("slopfile-research-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
  }

  fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]).unwrap();
    git(dir, &["config", "user.email", "test@test"]).unwrap();
    git(dir, &["config", "user.name", "test"]).unwrap();
    std::fs::write(dir.join("base.txt"), "one\n").unwrap();
    git(dir, &["add", "-A"]).unwrap();
    git(dir, &["commit", "-q", "-m", "init"]).unwrap();
  }

  #[test]
  fn score_subject_roundtrip() {
    assert_eq!(parse_score_subject("research: iter 7 score=0.42"), Some(0.42));
    assert_eq!(parse_score_subject("research: iter 3 score=12"), Some(12.0));
    assert_eq!(parse_score_subject("no score here"), None);
    assert_eq!(parse_score_subject(""), None);
  }

  #[test]
  fn better_respects_direction_and_baseline() {
    assert!(is_better(1.0, None, false), "first measurement always wins");
    assert!(is_better(-5.0, None, true));
    assert!(is_better(0.4, Some(0.5), false), "minimize: lower wins");
    assert!(!is_better(0.6, Some(0.5), false));
    assert!(!is_better(0.5, Some(0.5), false), "equal is not an improvement");
    assert!(is_better(9.0, Some(8.0), true), "maximize: higher wins");
    assert!(!is_better(7.0, Some(8.0), true));
  }

  #[test]
  fn eval_output_parsing() {
    assert_eq!(parse_eval_output("0.123\n"), Some(0.123));
    assert_eq!(parse_eval_output("  42  "), Some(42.0));
    assert_eq!(parse_eval_output("\n\n7.5\n"), Some(7.5));
    assert_eq!(parse_eval_output("last thing:\n13.25"), Some(13.25));
    assert_eq!(parse_eval_output("garbage"), None);
    assert_eq!(parse_eval_output(""), None);
    assert_eq!(parse_eval_output("not a number\nstill not"), None);
  }

  #[test]
  fn sanitize_restores_baseline_and_drops_untracked() {
    let dir = tmp("sanitize");
    init_repo(&dir);

    std::fs::write(dir.join("base.txt"), "baseline\n").unwrap();
    git(&dir, &["add", "-A"]).unwrap();
    git(&dir, &["commit", "-q", "-m", "research: iter 1 score=5"]).unwrap();
    git(&dir, &["tag", BASELINE_TAG]).unwrap();

    std::fs::write(dir.join("base.txt"), "half-finished mutation\n").unwrap();
    std::fs::write(dir.join("untracked.rs"), "fn main() {}\n").unwrap();

    sanitize(&dir).unwrap();

    assert_eq!(std::fs::read_to_string(dir.join("base.txt")).unwrap(), "baseline\n");
    assert!(!dir.join("untracked.rs").exists(), "untracked files wiped");
    assert_eq!(read_best_score(&dir).unwrap(), Some(5.0));

    std::fs::write(dir.join("base.txt"), "rogue\n").unwrap();
    git(&dir, &["add", "-A"]).unwrap();
    git(&dir, &["commit", "-q", "-m", "rogue commit"]).unwrap();
    sanitize(&dir).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("base.txt")).unwrap(), "baseline\n");

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn commit_baseline_moves_tag_and_score() {
    let dir = tmp("commit");
    init_repo(&dir);
    assert_eq!(read_best_score(&dir).unwrap(), None, "no tag yet");

    std::fs::write(dir.join("impl.txt"), "v1\n").unwrap();
    commit_baseline(&dir, 1, 10.5).unwrap();
    assert_eq!(read_best_score(&dir).unwrap(), Some(10.5));

    std::fs::write(dir.join("impl.txt"), "v2\n").unwrap();
    commit_baseline(&dir, 2, 8.25).unwrap();
    assert_eq!(read_best_score(&dir).unwrap(), Some(8.25));
    let head = git(&dir, &["rev-parse", "HEAD"]).unwrap();
    let tagged = git(&dir, &["rev-parse", &format!("refs/tags/{BASELINE_TAG}")]).unwrap();
    assert_eq!(head, tagged, "tag tracks the newest best");

    commit_baseline(&dir, 3, 8.0).unwrap();
    assert_eq!(read_best_score(&dir).unwrap(), Some(8.0));

    std::fs::remove_dir_all(&dir).ok();
  }

  #[tokio::test]
  async fn run_eval_spawns_parses_and_times_out() {
    let dir = tmp("evalrun");
    let mut opts = ResearchOpts {
      workdir: dir.clone(),
      ..Default::default()
    };

    std::fs::write(dir.join("out.sh"), "#!/bin/sh\necho 42.5\n").unwrap();
    opts.eval_cmd = "sh out.sh".into();
    assert_eq!(run_eval(&opts).await.unwrap(), 42.5);

    opts.eval_cmd = "echo garbage-only".into();
    assert!(run_eval(&opts).await.is_err(), "no number -> error");

    opts.eval_cmd = "exit 3".into();
    let err = run_eval(&opts).await.unwrap_err();
    assert!(err.contains("exit 3"), "{err}");

    opts.eval_cmd = "sleep 30".into();
    opts.eval_timeout_secs = 1;
    let err = run_eval(&opts).await.unwrap_err();
    assert!(err.contains("timed out"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn seeded_transcript_is_armed_and_terminates_correctly() {
    let dir = tmp("seed");
    let opts = ResearchOpts {
      transcripts_dir: dir.clone(),
      model: "m".into(),
      ..Default::default()
    };

    assert_eq!(next_iter_number(&dir), 1, "empty dir starts at 1");

    let agents = "make it fast\n";
    let path = seed_transcript(&opts, agents, "", None, 1).unwrap();
    assert_eq!(path.file_name().unwrap(), "research-001.md");
    assert_eq!(next_iter_number(&dir), 2, "next number skips existing files");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("```toml\nmodel = \"m\"\n"));
    assert!(text.contains("\"solve\""), "solve tool offered: {text}");
    assert!(
      text.contains("tools = [\"read\", \"write\", \"edit\", \"bash\", \"grep\", \"find\", \"ls\", \"question\", \"solve\"]")
        && text.contains("tools_excluded = [\"question\"]"),
      "subagent swapped for solve: {text}"
    );
    assert!(text.contains("--- BEGIN AGENTS.md ---\nmake it fast"), "{text}");
    assert!(text.contains("(JOURNAL.md does not exist yet"), "{text}");
    assert!(text.contains("No baseline exists yet"), "{text}");
    assert!(text.ends_with("<!-- slopfile:send -->\n"), "armed immediately: {text}");

    assert!(!iteration_finished(&text));

    let done = format!(
      "```toml\nmodel = \"m\"\n```\n# Session\nsys\n\n### You\ngo\n\n### Assistant\ntried a thing\n{}\n",
      crate::lex::DONE_MARKER
    );
    assert!(iteration_finished(&done));

    let dangling = "```toml\nmodel = \"m\"\n```\n# Session\nsys\n\n### You\ngo\n\n### Assistant\npartial thought".to_string();
    assert!(!iteration_finished(&dangling));

    let queued = format!("{done}\ncome back\n");
    assert!(!iteration_finished(&queued));

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn solve_marker_is_daemon_owned_and_never_a_draft() {
    use crate::lex::{lex, TokKind};
    let text = format!(
      "### Assistant\ndone deal\n{}\n{}\n",
      crate::lex::SOLVED_MARKER,
      crate::lex::DONE_MARKER
    );
    let kinds: Vec<TokKind> = lex(&text).into_iter().map(|t| t.kind).collect();
    assert!(kinds.contains(&TokKind::Solved), "marker lexed as Solved: {kinds:?}");
    assert!(crate::transcript::tail_user_text(&text).is_none(), "solved marker is not user text");
    assert!(crate::transcript::draft_tail(&text).is_none(), "solved marker is not a draft");
  }

  #[test]
  fn journal_seed_keeps_tail_when_huge() {
    let big = (0..2000).map(|i| format!("entry {i}: did something\n")).collect::<String>();
    let out = truncate_head(&big, 500);
    assert!(out.chars().count() < 600, "capped to ~max chars");
    assert!(out.contains("[earlier journal entries truncated]"));
    assert!(out.contains("entry 1999"), "tail preserved");
    assert!(!out.contains("entry 0:"), "head dropped");

    let small = "tiny\n".to_string();
    assert_eq!(truncate_head(&small, 500), small);
  }

  #[test]
  fn append_log_writes_one_json_line_per_call() {
    let dir = tmp("log");
    append_log(&dir, 1, Some(3.0), None, true, false, None).unwrap();
    append_log(&dir, 2, None, Some(3.0), false, false, Some("boom".into())).unwrap();

    let content = std::fs::read_to_string(dir.join("research-log.jsonl")).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2, "{content}");

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["iter"], 1);
    assert_eq!(first["score"], 3.0);
    assert_eq!(first["kept"], true);
    assert!(first.get("error").is_none(), "no error key when absent");

    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["error"], "boom");
    assert_eq!(second["score"], serde_json::Value::Null);

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn research_session_template_arms_with_send_marker() {
    let tpl = research_session_template("qwen", "do the thing\nsecond line\n");
    assert!(tpl.contains("model = \"qwen\""));
    assert!(tpl.contains(RESEARCH_SYSTEM_PROMPT));
    assert!(
      tpl.ends_with("### You\ndo the thing\nsecond line\n\n<!-- slopfile:send -->\n"),
      "tail: {}", &tpl[tpl.len().saturating_sub(120)..]
    );

    let pf = crate::transcript::parse_file(&tpl);
    assert_eq!(pf.history.len(), 2, "system + seeded user turn: {:?}", pf.history);
    assert_eq!(pf.history[0].role, crate::types::Role::System);
    assert_eq!(pf.history[1].role, crate::types::Role::User);
    assert!(
      pf.history[1]
        .content
        .as_deref()
        .is_some_and(|c| c.contains("do the thing")),
      "seed prompt becomes the user turn"
    );
  }
}
