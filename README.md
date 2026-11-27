# 🌊 Vibefleet

A [ratatui](https://github.com/ratatui-org/ratatui) **control room** for orchestrating a *fleet* of AI coding agents that work in parallel git worktrees — built for vibe coders who want to supervise, not babysit.

Flagsmith drives both the product rollout and per-agent behavior.

## What makes it different

Most "AI coding" TUIs are a single chat box. Vibefleet is a **fleet cockpit**:

- **Task DAG** (left) — dependency-aware; tasks become `Ready` only when their deps `Merged`.
- **Agent fleet** (center) — multiple agents, each bound to its own worktree + branch, with live status and per-agent token/cost.
- **Live log** (right) — streaming fleet events.
- **Vibe report** (footer) — auto-generated per task: files changed, test delta, risk level.
- **Header** — fleet-wide token/cost totals and the *active flag values* in effect.

## Flagsmith integration

Flags are first-class and used in two places:

1. **Product rollout** — `fleet.max_parallel`, `vibe_report.enabled`, `fleet.auto_merge_below`.
2. **Per-agent behavior** — `agent.mock.mode` (`deterministic`|`chaotic`), `agent.risk_threshold`.

By default Vibefleet ships an **offline evaluator** (`LocalFlags`) with sensible defaults and percentage-rollout support, so it runs with zero network. To use a real Flagsmith environment:

```sh
cargo run --features live-flagsmith
# provide your environment key + base URL in FlagsmithClient::new(...)
```

The `FeatureFlags` trait means the rest of the app never knows which backend is active.

## Plugging in a real agent

`AgentBackend` (in `src/agent.rs`) is the seam. Ship a deterministic `MockAgent` today; implement the trait for `aichat`, `llm`, or an HTTP LLM API and swap it in `App::new` — the TUI and orchestrator don't change.

## Keys

**Global:** `Shift+Tab` cycles modes (Fleet → Prompt → Editor).

**Fleet mode:**
```
space  pause/resume scheduling     s      step (schedule once)
tab    cycle focus                 ↑/↓    move selection
a      add agent                   r      toggle vibe-report flag
i      enter prompt mode           e      enter editor mode
q      quit
```

**Prompt mode** (the input section — where you type a prompt):
```
Enter  submit → enqueues a fleet task AND streams code into the Generation panel
Esc    back to Fleet
shift+tab  cycle mode
```

**Editor mode** (a real code editor — `ratatui-textarea`, synced to the
active agent's project worktree):
```
Ctrl+S  save file to the active agent's worktree (creates a checkpoint)
Ctrl+Z  undo last save
Ctrl+O  open the file picker
Esc     back to Fleet
shift+tab  cycle mode
```

**Files mode** (file picker scoped to the active agent's worktree):
```
type to filter…  Enter=open first match  Esc=back
```

**Command palette** (`p`, or Shift+Tab to it):
```
check  — cargo check + diagnostics pane
test   — run the project tests
diff   — git diff stat (files, +ins/-del)
files  — open file picker
editor — go to editor
prompt — go to prompt
```

The **Generation panel** streams code live. The **Diagnostics pane** shows
`cargo check` errors/warnings + git diff stat, and the header shows a live
**token/cost budget** that auto-pauses scheduling when exceeded.

## What's built for vibe coders

- **Editor synced to the active agent's worktree** — open/edit/save real files,
  with a file picker, dirty tracking, and undo checkpoints.
- **Diagnostics pane** — `cargo check` errors/warnings inline, plus git diff stat.
- **One-key tests** (`t`) and **check** (`c`) from anywhere in Fleet.
- **Command palette** (`p`) for check/test/diff/files/mode switches.
- **Budget guard** — token/cost ceiling auto-pauses the fleet (Flagsmith-tunable).

## Build & test

```sh
cargo build
cargo test      # 5 tests: flags rollout/defaults + orchestrator scheduling
cargo run
```

## Architecture

```
src/
  main.rs         entrypoint, file logging
  domain.rs       Task/Project/Agent/Event/VibeReport models
  flags.rs        FeatureFlags trait + LocalFlags (offline) + FlagsmithClient (feature)
  agent.rs        AgentBackend trait + MockAgent
  orchestrator.rs scheduler: worktrees, token/cost, event log, flag-bounded concurrency
  seed.rs         demo project (4-node DAG)
  tui.rs          ratatui UI + event loop
```

## Status

Runnable vertical slice: real layout, interactions, mock agents, Flagsmith abstraction, tests. Real LLM backends and live Flagsmith are one impl / one feature-flag away.