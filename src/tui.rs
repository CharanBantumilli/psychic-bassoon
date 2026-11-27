//! The ratatui control room with an interactive prompt + code editor.
//!
//! Modes (cycle with Shift+Tab / BackTab):
//!   - Fleet   : the multi-agent cockpit (task DAG | fleet | generation + live log)
//!   - Prompt  : type a prompt, Enter to submit -> streams into the Generation panel
//!   - Editor  : a real code editor (ratatui-textarea); Ctrl+S "saves" to the gen buffer
//!
//! Keys (global): Shift+Tab cycle mode
//! Prompt mode: Enter submit, Esc back to Fleet
//! Editor mode: Ctrl+S save, Esc back to Fleet

use crate::agent::MockAgent;
use crate::domain::{AgentStatus, EventLevel, TaskState};
use crate::flags::{keys, FeatureFlags, FlagValue, LocalFlags};
use crate::orchestrator::{FleetMsg, FleetState, GenMsg, Orchestrator};
use crate::seed::demo_project;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{backend::CrosstermBackend, Frame, Terminal};
use ratatui_textarea::TextArea;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Fleet,
    Prompt,
    Editor,
    Files,
    Palette,
}

impl Mode {
    fn next(self) -> Mode {
        match self {
            Mode::Fleet => Mode::Prompt,
            Mode::Prompt => Mode::Editor,
            Mode::Editor => Mode::Files,
            Mode::Files => Mode::Palette,
            Mode::Palette => Mode::Fleet,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Mode::Fleet => "FLEET",
            Mode::Prompt => "PROMPT",
            Mode::Editor => "EDITOR",
            Mode::Files => "FILES",
            Mode::Palette => "PALETTE",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Tasks,
    Fleet,
    Log,
}

pub struct App {
    pub state: FleetState,
    pub mode: Mode,
    pub focus: Focus,
    pub selected_task: usize,
    pub paused: bool,
    pub flags: Arc<LocalFlags>,

    /// Prompt input buffer.
    prompt: String,
    /// Streamed generation output.
    generation: String,
    generating: bool,
    /// Editor buffer.
    editor: TextArea<'static>,
    /// Path (relative to project root) currently open in the editor.
    open_file: Option<String>,
    /// Dirty flag: editor has unsaved changes.
    dirty: bool,
    /// File-picker filter string.
    file_filter: String,
    /// Parsed diagnostics from the last `cargo check`.
    diagnostics: Vec<crate::workspace::Diagnostic>,
    /// Last test summary line.
    test_summary: String,
    /// Git diff stat: (files, +ins, -del).
    diff_stat: (usize, i64, i64),
    /// Command palette filter.
    palette_filter: String,
    /// Token budget guard (USD). Scheduling pauses when exceeded.
    budget_usd: f64,
    budget_hit: bool,
    /// Undo stack of (file, previous contents) for Ctrl+Z in editor.
    undo: Vec<(String, String)>,

    msg_rx: mpsc::UnboundedReceiver<FleetMsg>,
    gen_rx: mpsc::UnboundedReceiver<GenMsg>,
    gen_tx: mpsc::UnboundedSender<GenMsg>,
    orch: Orchestrator,
}

impl App {
    pub fn new() -> Self {
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (gen_tx, gen_rx) = mpsc::unbounded_channel();
        let flags = Arc::new(LocalFlags::new());
        let backend: Arc<dyn crate::agent::AgentBackend> = Arc::new(MockAgent::new("mock-coder-v1"));
        let project = demo_project();
        let mut orch = Orchestrator::new(project, flags.clone(), backend, msg_tx);

        orch.add_agent(
            "ada".into(),
            "mock-coder-v1".into(),
            "/tmp/acme-vibe-app/.wt/ada".into(),
            "agent/ada".into(),
        );
        orch.add_agent(
            "linus".into(),
            "mock-coder-v1".into(),
            "/tmp/acme-vibe-app/.wt/linus".into(),
            "agent/linus".into(),
        );

        let state = orch.state().clone();
        let mut editor = TextArea::default();
        editor.set_block(styled_block("editor · Ctrl+S save · Esc back", Color::Green));
        Self {
            state,
            mode: Mode::Fleet,
            focus: Focus::Tasks,
            selected_task: 0,
            paused: false,
            flags,
            prompt: String::new(),
            generation: String::new(),
            generating: false,
            editor,
            open_file: None,
            dirty: false,
            file_filter: String::new(),
            diagnostics: Vec::new(),
            test_summary: String::new(),
            diff_stat: (0, 0, 0),
            palette_filter: String::new(),
            budget_usd: 5.0,
            budget_hit: false,
            undo: Vec::new(),
            msg_rx,
            gen_rx,
            gen_tx,
            orch,
        }
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.orch.apply(msg);
        }
        while let Ok(g) = self.gen_rx.try_recv() {
            match g {
                GenMsg::Chunk(s) => self.generation.push_str(&s),
                GenMsg::Done => self.generating = false,
            }
        }
        self.state = self.orch.state().clone();
    }

    fn schedule(&mut self) {
        if self.paused {
            return;
        }
        // Budget guard: pause scheduling if cost exceeds budget.
        if self.state.total_cost() >= self.budget_usd {
            if !self.budget_hit {
                self.orch.state_mut().push_event(
                    EventLevel::Warn,
                    format!("budget ${:.2} hit — scheduling paused", self.budget_usd),
                );
                self.budget_hit = true;
            }
            self.paused = true;
            return;
        }
        self.orch.schedule();
        self.state = self.orch.state().clone();
    }

    /// The active agent is the selected one in the fleet list, else the first.
    fn active_agent(&self) -> Option<&crate::domain::Agent> {
        self.state
            .agents
            .get(self.selected_task.min(self.state.agents.len().saturating_sub(1)))
            .or_else(|| self.state.agents.first())
    }

    /// Project root for the active agent (its worktree), falling back to repo.
    fn project_root(&self) -> std::path::PathBuf {
        if let Some(a) = self.active_agent() {
            let p = std::path::PathBuf::from(&a.worktree_path);
            if p.exists() {
                return p;
            }
        }
        std::path::PathBuf::from(&self.state.project.repo_path)
    }

    /// Open a file from the active agent's project into the editor.
    fn open_file(&mut self, rel: &str) {
        let root = self.project_root();
        match crate::workspace::read_file(&root, rel) {
            Ok(contents) => {
                self.editor = TextArea::from(
                    contents.lines().map(String::from).collect::<Vec<_>>(),
                );
                let title = format!("editor · {rel} · Ctrl+S save");
                self.editor.set_block(styled_block(title, Color::Green));
                self.open_file = Some(rel.to_string());
                self.dirty = false;
                self.mode = Mode::Editor;
            }
            Err(e) => {
                self.orch
                    .state_mut()
                    .push_event(EventLevel::Error, format!("open failed: {e}"));
            }
        }
    }

    /// Save the editor buffer back to the active agent's project file.
    fn save_file(&mut self) {
        let Some(rel) = self.open_file.clone() else {
            self.orch
                .state_mut()
                .push_event(EventLevel::Warn, "no file open to save");
            return;
        };
        let root = self.project_root();
        // Checkpoint for undo.
        if let Ok(prev) = crate::workspace::read_file(&root, &rel) {
            self.undo.push((rel.clone(), prev));
        }
        let text = self.editor.lines().join("\n");
        match crate::workspace::write_file(&root, &rel, &text) {
            Ok(()) => {
                self.dirty = false;
                self.diff_stat = crate::workspace::git_diff_stat(&root);
                self.orch.state_mut().push_event(
                    EventLevel::Success,
                    format!("saved {rel} ({} files changed)", self.diff_stat.0),
                );
            }
            Err(e) => {
                self.orch
                    .state_mut()
                    .push_event(EventLevel::Error, format!("save failed: {e}"));
            }
        }
    }

    /// Undo the last save (restore previous contents).
    fn undo_save(&mut self) {
        if let Some((rel, prev)) = self.undo.pop() {
            let root = self.project_root();
            let _ = crate::workspace::write_file(&root, &rel, &prev);
            self.open_file(&rel);
            self.orch
                .state_mut()
                .push_event(EventLevel::Info, format!("undid save of {rel}"));
        }
    }

    /// Run cargo check in the project and store diagnostics.
    fn run_check(&mut self) {
        let root = self.project_root();
        self.diagnostics = crate::workspace::run_check(&root);
        let errs = self
            .diagnostics
            .iter()
            .filter(|d| d.severity == crate::workspace::DiagSeverity::Error)
            .count();
        self.orch.state_mut().push_event(
            if errs == 0 { EventLevel::Success } else { EventLevel::Error },
            format!("cargo check: {errs} errors, {} total", self.diagnostics.len()),
        );
    }

    /// Run the project tests and store a summary.
    fn run_tests(&mut self) {
        let root = self.project_root();
        let (p, f, lines) = crate::workspace::run_tests(&root);
        self.test_summary = lines
            .last()
            .cloned()
            .unwrap_or_else(|| format!("{p} passed, {f} failed"));
        self.orch.state_mut().push_event(
            if f == 0 { EventLevel::Success } else { EventLevel::Error },
            format!("tests: {p} passed, {f} failed"),
        );
    }

    fn submit_prompt(&mut self) {
        let prompt = self.prompt.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        // 1) Enqueue as a fleet task, 2) stream a generation.
        self.orch.add_prompt_task(prompt.clone());
        self.generation.clear();
        self.generating = true;
        self.orch.start_generation(prompt, self.gen_tx.clone());
        self.prompt.clear();
        self.state = self.orch.state().clone();
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        // Global: Shift+Tab cycles mode.
        if key.code == KeyCode::BackTab {
            self.mode = self.mode.next();
            return true;
        }

        match self.mode {
            Mode::Fleet => self.on_key_fleet(key),
            Mode::Prompt => self.on_key_prompt(key),
            Mode::Editor => self.on_key_editor(key),
            Mode::Files => self.on_key_files(key),
            Mode::Palette => self.on_key_palette(key),
        }
    }

    fn on_key_fleet(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Tasks => Focus::Fleet,
                    Focus::Fleet => Focus::Log,
                    Focus::Log => Focus::Tasks,
                };
            }
            KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Char('s') => {
                self.paused = true;
                self.orch.schedule();
                self.state = self.orch.state().clone();
            }
            KeyCode::Char('a') => {
                let n = self.state.agents.len() + 1;
                self.orch.add_agent(
                    format!("agent{n}"),
                    "mock-coder-v1".into(),
                    format!("/tmp/acme-vibe-app/.wt/agent{n}"),
                    format!("agent/{n}"),
                );
                self.state = self.orch.state().clone();
            }
            KeyCode::Char('r') => {
                let id = self.flags_identity();
                let cur = self.flags.get_bool(keys::VIBE_REPORT_ENABLED, &id, true);
                self.flags.set(keys::VIBE_REPORT_ENABLED, FlagValue::Bool(!cur));
            }
            KeyCode::Char('i') => self.mode = Mode::Prompt,
            KeyCode::Char('e') => self.mode = Mode::Editor,
            KeyCode::Char('f') => self.mode = Mode::Files,
            KeyCode::Char('p') => self.mode = Mode::Palette,
            KeyCode::Char('c') => self.run_check(),
            KeyCode::Char('t') => self.run_tests(),
            KeyCode::Char('d') => {
                self.diff_stat = crate::workspace::git_diff_stat(&self.project_root());
            }
            KeyCode::Char('u') => self.undo_save(),
            KeyCode::Up => self.move_sel(1),
            KeyCode::Down => self.move_sel(-1),
            _ => {}
        }
        true
    }

    fn on_key_prompt(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Fleet,
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Backspace => {
                self.prompt.pop();
            }
            KeyCode::Char(c) => self.prompt.push(c),
            _ => {}
        }
        true
    }

    fn on_key_editor(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Fleet;
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => {
                    self.save_file();
                    self.mode = Mode::Fleet;
                    return true;
                }
                KeyCode::Char('z') => {
                    self.undo_save();
                    return true;
                }
                KeyCode::Char('o') => {
                    self.mode = Mode::Files;
                    return true;
                }
                _ => {}
            }
        }
        let before: String = self.editor.lines().join("\n");
        self.editor.input(ratatui_textarea::Input::from(key));
        let after: String = self.editor.lines().join("\n");
        if before != after {
            self.dirty = true;
        }
        true
    }

    fn on_key_files(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Fleet,
            KeyCode::Enter => {
                let root = self.project_root();
                let files = crate::workspace::list_files(&root, &self.file_filter);
                if let Some(first) = files.first() {
                    let f = first.clone();
                    self.open_file(&f);
                }
            }
            KeyCode::Backspace => {
                self.file_filter.pop();
            }
            KeyCode::Char(c) => self.file_filter.push(c),
            _ => {}
        }
        true
    }

    fn on_key_palette(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Fleet,
            KeyCode::Enter => {
                let cmd = self.palette_filter.trim().to_lowercase();
                self.palette_filter.clear();
                self.mode = Mode::Fleet;
                match cmd.as_str() {
                    "check" => self.run_check(),
                    "test" | "tests" => self.run_tests(),
                    "diff" => {
                        self.diff_stat = crate::workspace::git_diff_stat(&self.project_root());
                    }
                    "files" => self.mode = Mode::Files,
                    "editor" => self.mode = Mode::Editor,
                    "prompt" => self.mode = Mode::Prompt,
                    "" => {}
                    other => {
                        self.orch.state_mut().push_event(
                            EventLevel::Warn,
                            format!("unknown command: {other}"),
                        );
                    }
                }
            }
            KeyCode::Backspace => {
                self.palette_filter.pop();
            }
            KeyCode::Char(c) => self.palette_filter.push(c),
            _ => {}
        }
        true
    }

    fn flags_identity(&self) -> crate::flags::Identity {
        crate::flags::Identity::default()
    }

    fn move_sel(&mut self, delta: isize) {
        let len = self.state.project.tasks.len();
        if len == 0 {
            return;
        }
        let cur = self.selected_task as isize + delta;
        self.selected_task = cur.rem_euclid(len as isize) as usize;
    }
}

// ---- rendering ------------------------------------------------------------

fn styled_block(title: impl Into<String>, border: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::default().add_modifier(Modifier::BOLD),
        ))
}

fn state_color(s: TaskState) -> Color {
    match s {
        TaskState::Merged => Color::Green,
        TaskState::Running => Color::Cyan,
        TaskState::Review => Color::Yellow,
        TaskState::Ready => Color::Magenta,
        TaskState::Pending => Color::DarkGray,
        TaskState::Failed => Color::Red,
    }
}

fn agent_color(s: AgentStatus) -> Color {
    match s {
        AgentStatus::Working => Color::Cyan,
        AgentStatus::Review => Color::Yellow,
        AgentStatus::Done => Color::Green,
        AgentStatus::Failed => Color::Red,
        AgentStatus::Idle => Color::DarkGray,
    }
}

impl App {
    fn render(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
            .split(f.area());

        self.render_header(f, chunks[0]);
        match self.mode {
            Mode::Fleet => self.render_fleet_body(f, chunks[1]),
            Mode::Prompt => self.render_prompt_body(f, chunks[1]),
            Mode::Editor => self.render_editor_body(f, chunks[1]),
            Mode::Files => self.render_files_body(f, chunks[1]),
            Mode::Palette => self.render_palette_body(f, chunks[1]),
        }
        self.render_footer(f, chunks[2]);
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let id = self.flags_identity();
        let maxp = self.flags.get_i64(keys::MAX_PARALLEL, &id, 3);
        let vr = self.flags.get_bool(keys::VIBE_REPORT_ENABLED, &id, true);
        let mode = self.flags.get_str(keys::AGENT_MOCK_MODE, &id, "deterministic");
        let status = if self.paused { "PAUSED" } else { "LIVE" };
        let line = Line::from(vec![
            Span::styled(
                format!(" 🌊 Vibefleet · {} ", self.state.project.name),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "tokens={} cost=${:.4} agents={} ",
                    self.state.total_tokens(),
                    self.state.total_cost(),
                    self.state.agents.len()
                ),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("[flags: parallel={maxp} reports={vr} mode={mode}] "),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("◀ {} ▶", self.mode.label()),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                status,
                Style::default().fg(if self.paused { Color::Yellow } else { Color::Green }),
            ),
        ]);
        f.render_widget(Paragraph::new(line).block(styled_block("control room", Color::Cyan)), area);
    }

    // -- Fleet body --
    fn render_fleet_body(&self, f: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(28),
                Constraint::Percentage(32),
                Constraint::Percentage(40),
            ])
            .split(area);
        self.render_tasks(f, cols[0]);
        self.render_fleet(f, cols[1]);

        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40),
                Constraint::Percentage(35),
                Constraint::Percentage(25),
            ])
            .split(cols[2]);
        self.render_generation(f, right[0]);
        self.render_log(f, right[1]);
        self.render_diagnostics(f, right[2]);
    }

    fn render_diagnostics(&self, f: &mut Frame, area: Rect) {
        let errs = self
            .diagnostics
            .iter()
            .filter(|d| d.severity == crate::workspace::DiagSeverity::Error)
            .count();
        let (files, ins, del) = self.diff_stat;
        let title = format!("diagnostics · {errs} err · diff {files}f +{ins}/-{del}");
        let items: Vec<ListItem> = self
            .diagnostics
            .iter()
            .take(20)
            .map(|d| {
                let color = match d.severity {
                    crate::workspace::DiagSeverity::Error => Color::Red,
                    crate::workspace::DiagSeverity::Warning => Color::Yellow,
                };
                ListItem::new(Line::from(Span::styled(
                    d.message.clone(),
                    Style::default().fg(color),
                )))
            })
            .collect();
        let list = List::new(items).block(styled_block(title, Color::Red));
        f.render_widget(list, area);
    }

    fn render_tasks(&self, f: &mut Frame, area: Rect) {
        let border = if self.focus == Focus::Tasks { Color::Cyan } else { Color::DarkGray };
        let items: Vec<ListItem> = self
            .state
            .project
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let dep = if t.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(" ({} deps)", t.depends_on.len())
                };
                let marker = if i == self.selected_task { "▶ " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(marker, Style::default().fg(Color::Yellow)),
                    Span::styled(
                        format!("{:?} ", t.state),
                        Style::default().fg(state_color(t.state)),
                    ),
                    Span::raw(format!("{}{}", t.title, dep)),
                ]))
            })
            .collect();
        f.render_widget(List::new(items).block(styled_block("task DAG", border)), area);
    }

    fn render_fleet(&self, f: &mut Frame, area: Rect) {
        let border = if self.focus == Focus::Fleet { Color::Cyan } else { Color::DarkGray };
        let items: Vec<ListItem> = self
            .state
            .agents
            .iter()
            .map(|a| {
                let task = a
                    .current_task
                    .and_then(|tid| self.state.project.tasks.iter().find(|t| t.id == tid))
                    .map(|t| t.title.as_str())
                    .unwrap_or("-");
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(format!("● {} ", a.name), Style::default().fg(agent_color(a.status))),
                        Span::styled(format!("{:?}", a.status), Style::default().fg(agent_color(a.status))),
                        Span::raw(format!(" · {} tok · ${:.4}", a.total_tokens(), a.cost_usd)),
                    ]),
                    Line::from(vec![
                        Span::styled(format!("    branch {} ", a.branch), Style::default().fg(Color::DarkGray)),
                        Span::raw(task),
                    ]),
                ])
            })
            .collect();
        f.render_widget(List::new(items).block(styled_block("agent fleet", border)), area);
    }

    fn render_generation(&self, f: &mut Frame, area: Rect) {
        let title = if self.generating {
            "generation · streaming…"
        } else {
            "generation"
        };
        let p = Paragraph::new(self.generation.clone())
            .block(styled_block(title, Color::Magenta))
            .wrap(Wrap { trim: false })
            .scroll((0, 0));
        f.render_widget(p, area);
    }

    fn render_log(&self, f: &mut Frame, area: Rect) {
        let border = if self.focus == Focus::Log { Color::Cyan } else { Color::DarkGray };
        let items: Vec<ListItem> = self
            .state
            .events
            .iter()
            .rev()
            .take(50)
            .map(|e| {
                let color = match e.level {
                    EventLevel::Info => Color::Gray,
                    EventLevel::Warn => Color::Yellow,
                    EventLevel::Error => Color::Red,
                    EventLevel::Success => Color::Green,
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{} ", e.message),
                    Style::default().fg(color),
                )))
            })
            .collect();
        f.render_widget(List::new(items).block(styled_block("live log", border)), area);
    }

    // -- Prompt body --
    fn render_prompt_body(&self, f: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(5)])
            .split(area);
        self.render_generation(f, rows[0]);

        let input = Paragraph::new(self.prompt.clone())
            .block(styled_block("prompt · Enter to run · Esc back", Color::Yellow))
            .wrap(Wrap { trim: false });
        f.render_widget(input, rows[1]);
    }

    // -- Editor body --
    fn render_editor_body(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(self.editor.widget(), area);
    }

    // -- Files body --
    fn render_files_body(&self, f: &mut Frame, area: Rect) {
        let root = self.project_root();
        let files = crate::workspace::list_files(&root, &self.file_filter);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);
        let items: Vec<ListItem> = files
            .iter()
            .take(100)
            .map(|f| ListItem::new(Line::from(Span::raw(f.clone()))))
            .collect();
        let title = format!("files · {} · active agent worktree", root.display());
        let list = List::new(items).block(styled_block(title, Color::Cyan));
        f.render_widget(list, rows[0]);
        let filter = Paragraph::new(self.file_filter.clone())
            .block(styled_block("filter · Enter open first · Esc back", Color::Yellow));
        f.render_widget(filter, rows[1]);
    }

    // -- Palette body --
    fn render_palette_body(&self, f: &mut Frame, area: Rect) {
        let cmds = [
            "check  — cargo check + diagnostics",
            "test   — run tests",
            "diff   — git diff stat",
            "files  — open file picker",
            "editor — go to editor",
            "prompt — go to prompt",
        ];
        let items: Vec<ListItem> = cmds
            .iter()
            .map(|c| ListItem::new(Line::from(Span::raw(*c))))
            .collect();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);
        let list = List::new(items).block(styled_block("command palette", Color::Magenta));
        f.render_widget(list, rows[0]);
        let input = Paragraph::new(self.palette_filter.clone())
            .block(styled_block("> Enter run · Esc back", Color::Yellow));
        f.render_widget(input, rows[1]);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let keys_line = match self.mode {
            Mode::Fleet => "shift+tab=mode  i=prompt  e=editor  f=files  p=palette  c=check  t=test  d=diff  u=undo  q=quit",
            Mode::Prompt => "type your prompt…  Enter=run (task + generate)  Esc=back  shift+tab=mode",
            Mode::Editor => "edit…  Ctrl+S=save  Ctrl+Z=undo  Ctrl+O=open  Esc=back  shift+tab=mode",
            Mode::Files => "type to filter…  Enter=open first  Esc=back  shift+tab=mode",
            Mode::Palette => "type a command (check/test/diff/files/editor/prompt)  Enter=run  Esc=back",
        };
        let left = Line::from(vec![
            Span::styled("keys ", Style::default().fg(Color::Yellow)),
            Span::raw(keys_line),
        ]);
        let p = Paragraph::new(vec![left])
            .block(styled_block("detail", Color::DarkGray))
            .wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }
}

// ---- terminal lifecycle ---------------------------------------------------

fn setup() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn teardown(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

pub async fn run() -> Result<()> {
    let mut terminal = setup()?;
    let mut app = App::new();
    app.schedule();
    let res = run_loop(&mut terminal, &mut app).await;
    teardown(&mut terminal)?;
    res
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let tick = Duration::from_millis(60);
    loop {
        terminal.draw(|f| app.render(f))?;
        app.drain();
        app.schedule();

        if event::poll(tick)? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && !app.on_key(k) {
                    break;
                }
            }
        }
    }
    Ok(())
}