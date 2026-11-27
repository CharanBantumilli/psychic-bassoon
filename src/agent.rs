//! Agent abstraction and a deterministic mock implementation.
//!
//! Real backends (aichat, llm, an HTTP LLM API, etc.) implement [`AgentBackend`]
//! and plug in without touching the TUI or orchestrator.

use crate::domain::{AgentStatus, RiskLevel, VibeReport};
use crate::flags::{keys, FeatureFlags, Identity};
use async_trait::async_trait;
use uuid::Uuid;

/// A chunk of progress emitted by an agent while working a task.
#[derive(Debug, Clone)]
pub struct AgentProgress {
    pub status: AgentStatus,
    pub message: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// The outcome of an agent finishing a task.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub status: AgentStatus,
    pub files_changed: usize,
    pub test_delta: i32,
    pub risk: RiskLevel,
    pub summary: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// A chunk of streamed code generation for the interactive prompt flow.
#[derive(Debug, Clone)]
pub struct GenChunk {
    pub text: String,
    pub done: bool,
}

/// A pluggable agent backend. Implementations run one task to completion,
/// streaming progress via the provided channel.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Human/model label shown in the UI.
    fn model(&self) -> &str;

    /// Execute `task_spec` for `task_id`, sending progress updates and
    /// returning a final outcome.
    async fn run(
        &self,
        task_id: Uuid,
        task_spec: &str,
        flags: &dyn FeatureFlags,
        identity: &Identity,
        progress: tokio::sync::mpsc::UnboundedSender<AgentProgress>,
    ) -> anyhow::Result<AgentOutcome>;

    /// Interactive prompt -> streamed code generation. Default: a short
    /// placeholder so new backends compile without implementing it.
    async fn generate(
        &self,
        prompt: &str,
        flags: &dyn FeatureFlags,
        identity: &Identity,
        out: tokio::sync::mpsc::UnboundedSender<GenChunk>,
    ) -> anyhow::Result<()> {
        let _ = (flags, identity);
        let _ = out.send(GenChunk {
            text: format!("// backend does not implement generate: {prompt}\n"),
            done: true,
        });
        Ok(())
    }
}

/// A deterministic, offline agent used for demos and tests.
///
/// Behavior is tunable via Flagsmith:
///   - `agent.mock.mode` = "deterministic" (default) | "chaotic"
///   - `agent.risk_threshold` influences reported risk
pub struct MockAgent {
    model: String,
}

impl MockAgent {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
        }
    }
}

#[async_trait]
impl AgentBackend for MockAgent {
    fn model(&self) -> &str {
        &self.model
    }

    async fn run(
        &self,
        _task_id: Uuid,
        task_spec: &str,
        flags: &dyn FeatureFlags,
        identity: &Identity,
        progress: tokio::sync::mpsc::UnboundedSender<AgentProgress>,
    ) -> anyhow::Result<AgentOutcome> {
        let chaotic = flags.get_str(keys::AGENT_MOCK_MODE, identity, "deterministic") == "chaotic";
        let risk_threshold = flags.get_i64(keys::AGENT_RISK_THRESHOLD, identity, 60);

        // Number of steps derived deterministically from the spec length.
        let steps = 3 + (task_spec.len() % 3) as u64;
        let mut tokens_in = 0u64;
        let mut tokens_out = 0u64;

        for step in 0..steps {
            let jitter = if chaotic { (step * 37) % 50 } else { 0 };
            let ti = 120 + step * 40 + jitter;
            let to = 80 + step * 25 + jitter;
            tokens_in += ti;
            tokens_out += to;

            let _ = progress.send(AgentProgress {
                status: AgentStatus::Working,
                message: format!("step {}/{} · editing", step + 1, steps),
                tokens_in: ti,
                tokens_out: to,
            });
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        }

        // Derive a stable pseudo-risk from spec content + threshold.
        let raw = (task_spec.bytes().map(|b| b as i64).sum::<i64>() % 100).unsigned_abs() as i64;
        let risk = if raw < 30 {
            RiskLevel::Low
        } else if raw < risk_threshold {
            RiskLevel::Medium
        } else {
            RiskLevel::High
        };

        let files_changed = 1 + (task_spec.len() % 4);
        let test_delta = if chaotic { -1 } else { 2 };
        let summary = format!("Implemented: {}", task_spec.lines().next().unwrap_or("task"));

        let _ = progress.send(AgentProgress {
            status: AgentStatus::Review,
            message: "awaiting review".into(),
            tokens_in: 0,
            tokens_out: 0,
        });

        Ok(AgentOutcome {
            status: AgentStatus::Done,
            files_changed,
            test_delta,
            risk,
            summary,
            tokens_in,
            tokens_out,
        })
    }

    async fn generate(
        &self,
        prompt: &str,
        flags: &dyn FeatureFlags,
        identity: &Identity,
        out: tokio::sync::mpsc::UnboundedSender<GenChunk>,
    ) -> anyhow::Result<()> {
        let chaotic = flags.get_str(keys::AGENT_MOCK_MODE, identity, "deterministic") == "chaotic";
        // Produce a deterministic, prompt-derived code block streamed line by line.
        let lang = if prompt.to_lowercase().contains("python") {
            "python"
        } else {
            "rust"
        };
        let lines: Vec<String> = match lang {
            "python" => vec![
                format!("# generated for: {prompt}"),
                "def solve():".into(),
                "    # TODO: implement".into(),
                "    return None".into(),
            ],
            _ => vec![
                format!("// generated for: {prompt}"),
                "pub fn solve() -> Result<()> {".into(),
                "    // TODO: implement".into(),
                "    Ok(())".into(),
                "}".into(),
            ],
        };
        for (i, line) in lines.iter().enumerate() {
            let jitter = if chaotic { (i * 13) % 20 } else { 0 };
            let _ = out.send(GenChunk {
                text: format!("{line}\n"),
                done: false,
            });
            tokio::time::sleep(std::time::Duration::from_millis(90 + jitter as u64)).await;
        }
        let _ = out.send(GenChunk { text: String::new(), done: true });
        Ok(())
    }
}

/// Build a [`VibeReport`] from an outcome when the feature is enabled.
pub fn maybe_report(outcome: &AgentOutcome, task_id: Uuid) -> Option<VibeReport> {
    Some(VibeReport {
        task_id,
        summary: outcome.summary.clone(),
        files_changed: outcome.files_changed,
        risk: outcome.risk,
        test_delta: outcome.test_delta,
    })
}