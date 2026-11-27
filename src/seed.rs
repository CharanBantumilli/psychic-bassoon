//! Seed data: a demo project with a small task DAG and a couple of agents.

use crate::domain::{Project, Task, TaskState};
use chrono::Utc;
use uuid::Uuid;

/// Build a demo project: a 4-node DAG (auth -> api -> ui, plus docs).
pub fn demo_project() -> Project {
    let now = Utc::now();
    let auth = Uuid::new_v4();
    let api = Uuid::new_v4();
    let ui = Uuid::new_v4();
    let docs = Uuid::new_v4();

    let tasks = vec![
        Task {
            id: auth,
            title: "Add user authentication".to_string(),
            spec: "Implement JWT login + refresh with a sqlite-backed user store.".to_string(),
            depends_on: vec![],
            state: TaskState::Ready,
            created_at: now,
        },
        Task {
            id: api,
            title: "Build REST API".to_string(),
            spec: "CRUD endpoints for projects with auth middleware wired in.".to_string(),
            depends_on: vec![auth],
            state: TaskState::Pending,
            created_at: now,
        },
        Task {
            id: ui,
            title: "Dashboard UI".to_string(),
            spec: "React dashboard consuming the REST API with optimistic updates.".to_string(),
            depends_on: vec![api],
            state: TaskState::Pending,
            created_at: now,
        },
        Task {
            id: docs,
            title: "Write docs".to_string(),
            spec: "Generate onboarding + API reference from code annotations.".to_string(),
            depends_on: vec![],
            state: TaskState::Ready,
            created_at: now,
        },
    ];

    Project {
        id: Uuid::new_v4(),
        name: "acme-vibe-app".into(),
        repo_path: "/tmp/acme-vibe-app".into(),
        tasks,
    }
}