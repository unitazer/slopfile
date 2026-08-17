//! Agent state machine driving one session file.
//!
//! States: idle → generating → tool_exec → waiting_answer → idle. The daemon
//! sweep loop calls `tick()` at a fixed interval; the agent progresses by
//! inspecting the parsed transcript.

use crate::api::LlamaClient;
use crate::transcript::ParsedFile;
use crate::types::{ChatMessage, LiveStatus, SessionConfig, StreamResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Generating,
    ToolExec,
    WaitingAnswer,
    Failed,
}

pub struct Agent {
    pub state: AgentState,
    pub client: LlamaClient,
    pub config: SessionConfig,
    /// Scratch: messages built from the file at generation start.
    pub messages: Vec<ChatMessage>,
    /// True while a chat_stream request is in flight.
    pub generating: bool,
    /// Most recent generation outcome (set by the daemon after the stream
    /// completes; the daemon routes from here).
    pub last_gen: Option<StreamResult>,
    pub last_error: Option<String>,
}

impl Agent {
    pub fn new(client: LlamaClient, config: SessionConfig) -> Self {
        Self {
            state: AgentState::Idle,
            client,
            config,
            messages: Vec::new(),
            generating: false,
            last_gen: None,
            last_error: None,
        }
    }

    /// Build the LLM context from a parsed file: system + history, plus a
    /// resume note when a dangling assistant turn exists.
    pub fn messages_from_file(&self, pf: &ParsedFile) -> Vec<ChatMessage> {
        let mut messages = pf.history.clone();
        if let Some(dangling) = &pf.dangling {
            if !dangling.partial_text.trim().is_empty() {
                messages.push(ChatMessage::user(format!(
                    "[resume] previous assistant output was interrupted; continue exactly where it stopped:\n```\n{}\n```",
                    dangling.partial_text.trim_end()
                )));
            }
        }
        messages
    }

    /// No `@done` below the newest `### You`, a dangling assistant turn, or
    /// unanswered questions — any of these mean the daemon has work.
    pub fn needs_generation(&self, pf: &ParsedFile) -> bool {
        pf.dangling.is_some() || !pf.pending_questions.is_empty()
    }

    pub fn start_generation(&mut self, pf: &ParsedFile) {
        self.messages = self.messages_from_file(pf);
        self.state = AgentState::Generating;
        self.generating = true;
    }

    /// Call after the stream finishes. Tool calls route to ToolExec (the
    /// daemon executes them), otherwise WaitingAnswer (text written to the
    /// tail by the daemon, then `@done`).
    pub fn finalize_generation(&mut self) {
        self.generating = false;
        match &self.last_gen {
            Some(g) if !g.tool_calls.is_empty() => self.state = AgentState::ToolExec,
            _ => self.state = AgentState::WaitingAnswer,
        }
    }

    pub fn resume(&mut self, pf: &ParsedFile) {
        self.start_generation(pf);
    }

    /// Compact status for the live status line.
    pub fn status_view(&self) -> LiveStatus {
        let phase = match self.state {
            AgentState::Idle => "idle",
            AgentState::Generating => "generating",
            AgentState::ToolExec => "tool",
            AgentState::WaitingAnswer => "waiting_answer",
            AgentState::Failed => "error",
        }
        .to_string();
        LiveStatus {
            phase,
            model: self.config.model.clone().unwrap_or_else(|| "?model".into()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::parse_file;
    use crate::types::{Role, ToolCall};

    fn pf_with_history() -> ParsedFile {
        parse_file(
            "```toml\nmodel = \"m\"\n```\n# Sys\nYou are X.\n\n### You\nhi\n\n### Assistant\nhello\n<!-- @done -->\n",
        )
    }

    #[test]
    fn idle_by_default() {
        let a = Agent::new(LlamaClient::new("http://x", "m"), SessionConfig::default());
        assert_eq!(a.state, AgentState::Idle);
        assert_eq!(a.status_view().phase, "idle");
    }

    #[test]
    fn messages_include_system_and_history() {
        let pf = pf_with_history();
        let a = Agent::new(LlamaClient::new("http://x", "m"), SessionConfig::default());
        let msgs = a.messages_from_file(&pf);
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[1].role, Role::User);
        assert_eq!(msgs[2].role, Role::Assistant);
    }

    #[test]
    fn dangling_adds_resume_hint_and_implies_work() {
        let pf = parse_file("### You\nhi\n\n### Assistant\npart of a response\n");
        assert!(pf.dangling.is_some());
        let a = Agent::new(LlamaClient::new("http://x", "m"), SessionConfig::default());
        let msgs = a.messages_from_file(&pf);
        assert!(msgs.last().unwrap().content.as_deref().unwrap().contains("[resume]"));
        assert!(a.needs_generation(&pf));
    }

    #[test]
    fn done_marker_means_no_work() {
        let a = Agent::new(LlamaClient::new("http://x", "m"), SessionConfig::default());
        assert!(!a.needs_generation(&pf_with_history()));
    }

    #[test]
    fn finalize_routes_tool_calls_to_toolexec() {
        let mut a = Agent::new(LlamaClient::new("http://x", "m"), SessionConfig::default());
        a.start_generation(&pf_with_history());
        a.last_gen = Some(StreamResult {
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            ..Default::default()
        });
        a.finalize_generation();
        assert_eq!(a.state, AgentState::ToolExec);
    }

    #[test]
    fn finalize_routes_plain_text_to_waiting() {
        let mut a = Agent::new(LlamaClient::new("http://x", "m"), SessionConfig::default());
        a.start_generation(&pf_with_history());
        a.last_gen = Some(StreamResult::default());
        a.finalize_generation();
        assert_eq!(a.state, AgentState::WaitingAnswer);
    }
}