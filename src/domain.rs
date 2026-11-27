//! Core domain types for the Vibefleet control room.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A coding task the fleet works on. Tasks form a DAG via `depends_on`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub title: String,
    pub spec: String,
    pub depends_on: Vec<Uuid>,
    pub state: TaskState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Pending,
    Ready,
    Running,
    Review,
    Merged,
    Failed,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Merged | TaskState::Failed)
    }
}

/// A project is a git repository the fleet operates on, plus its task DAG.
#[derive(Debug, Clone)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub repo_path: String,
    pub tasks: Vec<Task>,
}

impl Project {
    /// Tasks whose dependencies are all merged become `Ready`.
    pub fn recompute_ready(&mut self) {
        let merged: Vec<Uuid> = self
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Merged)
            .map(|t| t.id)
            .collect();
        for task in &mut self.tasks {
            if task.state == TaskState::Pending
                && task.depends_on.iter().all(|d| merged.contains(d))
            {
                task.state = TaskState::Ready;
            }
        }
    }

    pub fn ready_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.state == TaskState::Ready)
            .collect()
    }
}

/// Lifecycle of a single agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Working,
    Review,
    Done,
    Failed,
}

/// A fleet agent bound to an isolated git worktree.
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: Uuid,
    pub name: String,
    pub model: String,
    pub status: AgentStatus,
    pub worktree_path: String,
    pub branch: String,
    pub current_task: Option<Uuid>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Accumulated cost in USD, derived from token usage.
    pub cost_usd: f64,
}

impl Agent {
    pub fn total_tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }
}

/// A timestamped event in the fleet's activity log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetEvent {
    pub at: DateTime<Utc>,
    pub level: EventLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventLevel {
    Info,
    Warn,
    Error,
    Success,
}

/// The auto-generated "vibe report" for a finished task.
#[derive(Debug, Clone)]
pub struct VibeReport {
    pub task_id: Uuid,
    pub summary: String,
    pub files_changed: usize,
    pub risk: RiskLevel,
    pub test_delta: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}