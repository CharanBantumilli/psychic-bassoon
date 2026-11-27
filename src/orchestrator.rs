//! Fleet orchestrator: schedules ready tasks onto idle agents backed by
//! isolated git worktrees, tracks token/cost, and emits events.
//!
//! Concurrency is bounded by the `fleet.max_parallel` flag.

use crate::agent::{AgentBackend, AgentProgress, maybe_report};
use crate::domain::{
    Agent, AgentStatus, EventLevel, FleetEvent, Project, TaskState, VibeReport,
};
use crate::flags::{keys, FeatureFlags, Identity};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Rough per-token cost model (USD). Replace with real pricing as needed.
const COST_PER_1K_IN: f64 = 0.0005;
const COST_PER_1K_OUT: f64 = 0.0015;

/// Messages flowing from background agent tasks back to the orchestrator/UI.
pub enum FleetMsg {
    Progress {
        agent_id: Uuid,
        task_id: Uuid,
        progress: AgentProgress,
    },
    Done {
        agent_id: Uuid,
        task_id: Uuid,
        outcome: crate::agent::AgentOutcome,
    },
    Failed {
        agent_id: Uuid,
        task_id: Uuid,
        error: String,
    },
}

/// Streamed code-generation messages for the interactive prompt flow.
pub enum GenMsg {
    Chunk(String),
    Done,
}

/// Shared, cloneable state the UI reads and the orchestrator mutates.
#[derive(Clone)]
pub struct FleetState {
    pub project: Project,
    pub agents: Vec<Agent>,
    pub events: Vec<FleetEvent>,
    pub reports: Vec<VibeReport>,
    pub queue: VecDeque<Uuid>,
}

impl FleetState {
    pub fn new(project: Project) -> Self {
        Self {
            project,
            agents: Vec::new(),
            events: Vec::new(),
            reports: Vec::new(),
            queue: VecDeque::new(),
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.agents.iter().map(|a| a.total_tokens()).sum()
    }
    pub fn total_cost(&self) -> f64 {
        self.agents.iter().map(|a| a.cost_usd).sum()
    }
    pub fn push_event(&mut self, level: EventLevel, message: impl Into<String>) {
        self.events.push(FleetEvent {
            at: chrono::Utc::now(),
            level,
            message: message.into(),
        });
    }
}

/// The orchestrator owns scheduling. The UI holds a clone of the state and a
/// sender for control commands; updates flow back via `FleetMsg`.
pub struct Orchestrator {
    state: FleetState,
    flags: Arc<dyn FeatureFlags>,
    backend: Arc<dyn AgentBackend>,
    msg_tx: mpsc::UnboundedSender<FleetMsg>,
}

impl Orchestrator {
    pub fn new(
        project: Project,
        flags: Arc<dyn FeatureFlags>,
        backend: Arc<dyn AgentBackend>,
        msg_tx: mpsc::UnboundedSender<FleetMsg>,
    ) -> Self {
        let mut state = FleetState::new(project);
        state.project.recompute_ready();
        state.push_event(EventLevel::Info, "fleet initialized");
        Self {
            state,
            flags,
            backend,
            msg_tx,
        }
    }

    pub fn state(&self) -> &FleetState {
        &self.state
    }
    pub fn state_mut(&mut self) -> &mut FleetState {
        &mut self.state
    }

    /// Register an agent bound to a worktree.
    pub fn add_agent(&mut self, name: String, model: String, worktree_path: String, branch: String) {
        let id = Uuid::new_v4();
        self.state.agents.push(Agent {
            id,
            name: name.clone(),
            model,
            status: AgentStatus::Idle,
            worktree_path,
            branch,
            current_task: None,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
        });
        self.state
            .push_event(EventLevel::Success, format!("agent '{name}' joined the fleet"));
        let _ = id;
    }

    /// Turn a free-form prompt into a new Ready task (no deps) so the fleet
    /// can pick it up on the next schedule tick.
    pub fn add_prompt_task(&mut self, prompt: String) -> Uuid {
        let id = Uuid::new_v4();
        let title = prompt.lines().next().unwrap_or("task").chars().take(48).collect::<String>();
        self.state.project.tasks.push(crate::domain::Task {
            id,
            title,
            spec: prompt.clone(),
            depends_on: vec![],
            state: TaskState::Ready,
            created_at: chrono::Utc::now(),
        });
        self.state.push_event(EventLevel::Info, format!("new task from prompt: {prompt}"));
        id
    }

    /// Spawn a streaming code-generation run for `prompt`. Chunks arrive on
    /// `gen_tx`; the UI renders them in the generation panel.
    pub fn start_generation(
        &self,
        prompt: String,
        gen_tx: mpsc::UnboundedSender<GenMsg>,
    ) {
        let backend = self.backend.clone();
        let flags = self.flags.clone();
        let identity = Identity::default();
        tokio::spawn(async move {
            let (chunk_tx, mut chunk_rx) =
                mpsc::unbounded_channel::<crate::agent::GenChunk>();
            let fwd = gen_tx.clone();
            tokio::spawn(async move {
                while let Some(c) = chunk_rx.recv().await {
                    if c.done {
                        let _ = fwd.send(GenMsg::Done);
                    } else {
                        let _ = fwd.send(GenMsg::Chunk(c.text));
                    }
                }
            });
            if let Err(e) = backend.generate(&prompt, flags.as_ref(), &identity, chunk_tx).await {
                let _ = gen_tx.send(GenMsg::Chunk(format!("// error: {e}\n")));
                let _ = gen_tx.send(GenMsg::Done);
            }
        });
    }

    /// Schedule as many ready tasks as capacity (flag-bounded) allows.
    pub fn schedule(&mut self) {
        self.state.project.recompute_ready();
        let max_parallel = self
            .flags
            .get_i64(keys::MAX_PARALLEL, &Identity::default(), 3)
            .max(1) as usize;

        loop {
            let running = self
                .state
                .agents
                .iter()
                .filter(|a| a.status == AgentStatus::Working)
                .count();
            if running >= max_parallel {
                break;
            }
            let idle_idx = self
                .state
                .agents
                .iter()
                .position(|a| a.status == AgentStatus::Idle);
            let Some(ai) = idle_idx else { break };
            let Some(task) = self.state.project.ready_tasks().into_iter().next().cloned() else {
                break;
            };

            let agent_id = self.state.agents[ai].id;
            // Mark agent + task.
            self.state.agents[ai].status = AgentStatus::Working;
            self.state.agents[ai].current_task = Some(task.id);
            for t in &mut self.state.project.tasks {
                if t.id == task.id {
                    t.state = TaskState::Running;
                }
            }
            self.state.push_event(
                EventLevel::Info,
                format!("agent '{}' started task '{}'", self.state.agents[ai].name, task.title),
            );
            self.spawn_agent(agent_id, task.id, task.spec);
        }
    }

    fn spawn_agent(&self, agent_id: Uuid, task_id: Uuid, spec: String) {
        let backend = self.backend.clone();
        let flags = self.flags.clone();
        let msg_tx = self.msg_tx.clone();
        let identity = Identity {
            key: agent_id.to_string(),
            ..Default::default()
        };

        tokio::spawn(async move {
            let (p_tx, mut p_rx) = mpsc::unbounded_channel::<AgentProgress>();
            let fwd = msg_tx.clone();
            let fwd_task = task_id;
            let fwd_agent = agent_id;
            tokio::spawn(async move {
                while let Some(p) = p_rx.recv().await {
                    let _ = fwd.send(FleetMsg::Progress {
                        agent_id: fwd_agent,
                        task_id: fwd_task,
                        progress: p,
                    });
                }
            });

            match backend.run(task_id, &spec, flags.as_ref(), &identity, p_tx).await {
                Ok(outcome) => {
                    let _ = msg_tx.send(FleetMsg::Done {
                        agent_id,
                        task_id,
                        outcome,
                    });
                }
                Err(e) => {
                    let _ = msg_tx.send(FleetMsg::Failed {
                        agent_id,
                        task_id,
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    /// Apply an incoming message to state. Returns true if a report was added.
    pub fn apply(&mut self, msg: FleetMsg) {
        match msg {
            FleetMsg::Progress {
                agent_id,
                progress,
                ..
            } => {
                if let Some(a) = self.state.agents.iter_mut().find(|a| a.id == agent_id) {
                    a.status = progress.status;
                    a.tokens_in += progress.tokens_in;
                    a.tokens_out += progress.tokens_out;
                    a.cost_usd += progress.tokens_in as f64 / 1000.0 * COST_PER_1K_IN
                        + progress.tokens_out as f64 / 1000.0 * COST_PER_1K_OUT;
                }
                self.state
                    .push_event(EventLevel::Info, progress.message);
            }
            FleetMsg::Done {
                agent_id,
                task_id,
                outcome,
            } => {
                let auto_merge_below =
                    self.flags.get_i64(keys::AUTO_MERGE_BELOW, &Identity::default(), 30);
                let risk_val = match outcome.risk {
                    crate::domain::RiskLevel::Low => 0,
                    crate::domain::RiskLevel::Medium => 50,
                    crate::domain::RiskLevel::High => 100,
                };
                let merged = risk_val < auto_merge_below;

                if let Some(a) = self.state.agents.iter_mut().find(|a| a.id == agent_id) {
                    a.status = if merged { AgentStatus::Done } else { AgentStatus::Review };
                    a.current_task = None;
                    a.tokens_in += outcome.tokens_in;
                    a.tokens_out += outcome.tokens_out;
                    a.cost_usd += outcome.tokens_in as f64 / 1000.0 * COST_PER_1K_IN
                        + outcome.tokens_out as f64 / 1000.0 * COST_PER_1K_OUT;
                }
                for t in &mut self.state.project.tasks {
                    if t.id == task_id {
                        t.state = if merged { TaskState::Merged } else { TaskState::Review };
                    }
                }
                if self
                    .flags
                    .get_bool(keys::VIBE_REPORT_ENABLED, &Identity::default(), true)
                {
                    if let Some(r) = maybe_report(&outcome, task_id) {
                        self.state.push_event(
                            EventLevel::Success,
                            format!("vibe report: {} (risk {:?})", r.summary, r.risk),
                        );
                        self.state.reports.push(r);
                    }
                }
                self.state.push_event(
                    EventLevel::Success,
                    format!("task done · merged={merged} · files={}", outcome.files_changed),
                );
            }
            FleetMsg::Failed {
                agent_id, task_id, error,
            } => {
                if let Some(a) = self.state.agents.iter_mut().find(|a| a.id == agent_id) {
                    a.status = AgentStatus::Failed;
                    a.current_task = None;
                }
                for t in &mut self.state.project.tasks {
                    if t.id == task_id {
                        t.state = TaskState::Failed;
                    }
                }
                self.state.push_event(EventLevel::Error, format!("task failed: {error}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::MockAgent;
    use crate::flags::LocalFlags;
    use crate::seed::demo_project;

    fn setup() -> (Orchestrator, mpsc::UnboundedReceiver<FleetMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let flags: Arc<dyn FeatureFlags> = Arc::new(LocalFlags::new());
        let backend: Arc<dyn AgentBackend> = Arc::new(MockAgent::new("mock"));
        let orch = Orchestrator::new(demo_project(), flags, backend, tx);
        (orch, rx)
    }

    #[tokio::test]
    async fn schedule_respects_max_parallel() {
        let (mut orch, _rx) = setup();
        for i in 0..3 {
            orch.add_agent(format!("a{i}"), "mock".into(), format!("/wt/{i}"), format!("b{i}"));
        }
        orch.schedule();
        let working = orch
            .state()
            .agents
            .iter()
            .filter(|a| a.status == AgentStatus::Working)
            .count();
        assert!(working <= 3, "working={working} exceeds cap");
        assert!(working >= 1, "at least one agent should be working");
    }

    #[tokio::test]
    async fn ready_tasks_respect_dependencies() {
        let (orch, _rx) = setup();
        let ready = orch.state().project.ready_tasks().len();
        assert_eq!(ready, 2);
    }

    #[tokio::test]
    async fn prompt_adds_task_and_streams_generation() {
        let (mut orch, _rx) = setup();
        let before = orch.state().project.tasks.len();
        orch.add_prompt_task("build a python cli".into());
        assert_eq!(orch.state().project.tasks.len(), before + 1);

        let (gen_tx, mut gen_rx) = mpsc::unbounded_channel();
        orch.start_generation("build a python cli".into(), gen_tx);
        let mut got = String::new();
        let mut done = false;
        while let Ok(m) = gen_rx.try_recv() {
            match m {
                GenMsg::Chunk(s) => got.push_str(&s),
                GenMsg::Done => {
                    done = true;
                    break;
                }
            }
        }
        // Allow the spawned task to run briefly.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        while let Ok(m) = gen_rx.try_recv() {
            match m {
                GenMsg::Chunk(s) => got.push_str(&s),
                GenMsg::Done => done = true,
            }
        }
        assert!(done, "generation should complete");
        assert!(got.contains("python"), "generated code should mention the prompt: {got}");
    }
}