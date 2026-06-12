use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Stdout, Write},
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::{
    SynchronizedUpdate,
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

use crate::{config::Config, docker, project};

const SIDEBAR_WIDTH: u16 = 28;
const SIDEBAR_HEADER_ROWS: u16 = 2;
const SIDEBAR_FOOTER_ROWS: u16 = 2;
const SIDEBAR_SESSION_ROWS: u16 = 5;
const SCROLLBACK_ROWS_PER_TICK: usize = 3;
const FRAME_INTERVAL: Duration = Duration::from_millis(50);
const CODEX_STATUS_SUBMIT_DELAY: Duration = Duration::from_millis(100);
const STATUS_DISMISS_DELAY: Duration = Duration::from_millis(100);
const STATUS_CAPTURE_DELAY: Duration = Duration::from_secs(2);
const STATUS_INITIAL_DELAY: Duration = Duration::from_secs(5);
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(30);
const STATUS_READY_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_RETRY_DELAY: Duration = Duration::from_secs(2);
const RECENT_REPO_LIMIT: usize = 8;
const DISCOVERED_REPO_LIMIT: usize = 200;
const REPO_SEARCH_DEPTH: usize = 4;
const MAX_CLIPBOARD_IMAGE_BYTES: usize = 25 * 1024 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentKind {
    Claude,
    Codex,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollOwner {
    Outer,
    Child,
}

impl AgentKind {
    fn command(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("/usage"),
            Self::Codex => Some("/status"),
            Self::Other => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Other => "agent",
        }
    }

    fn status_submit_delay(self) -> Option<Duration> {
        (self == Self::Codex).then_some(CODEX_STATUS_SUBMIT_DELAY)
    }

    fn scroll_owner(self) -> ScrollOwner {
        match self {
            Self::Codex => ScrollOwner::Outer,
            Self::Claude | Self::Other => ScrollOwner::Child,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: (u16, u16),
    head: (u16, u16),
}

impl Selection {
    fn normalized(self) -> ((u16, u16), (u16, u16)) {
        if self.head < self.anchor {
            (self.head, self.anchor)
        } else {
            (self.anchor, self.head)
        }
    }

    fn is_empty(self) -> bool {
        self.anchor == self.head
    }
}

struct Session {
    id: String,
    name: String,
    repo: PathBuf,
    container: String,
    agent: AgentKind,
    agent_command: String,
    workdir: String,
    terminal: PtyProcess,
    selection: Option<Selection>,
    activity: AgentActivity,
    prompt_was_hidden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentActivity {
    Starting,
    Waiting,
    Working,
    Done,
}

impl AgentActivity {
    fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Waiting => "waiting",
            Self::Working => "working",
            Self::Done => "done",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct SavedSession {
    id: String,
    repo: PathBuf,
    #[serde(default)]
    agent: Option<String>,
}

struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    dirty: Arc<AtomicBool>,
    exited: Option<u32>,
}

struct StatusMonitor {
    agent: AgentKind,
    container: String,
    workdir: String,
    terminal: Option<PtyProcess>,
    phase: StatusPhase,
    next_refresh: Instant,
    has_capture: bool,
    retry_attempted: bool,
    percent_left: Option<u8>,
    summary: String,
    detail: String,
}

#[derive(Default)]
struct StatusMonitors {
    claude: Option<StatusMonitor>,
    codex: Option<StatusMonitor>,
}

enum StatusPhase {
    Idle,
    WaitingForReady(Instant),
    Dismissing(Instant),
    SubmitPending(Instant),
    Capturing(Instant),
    RetryPending(Instant),
}

#[derive(Debug, PartialEq, Eq)]
enum StatusRead {
    Success(UsageStatus),
    Retry,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
struct UsageStatus {
    percent_left: u8,
    summary: String,
}

impl PtyProcess {
    fn spawn(command: CommandBuilder, rows: u16, cols: u16) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let dirty = Arc::new(AtomicBool::new(true));
        let mut reader = pair.master.try_clone_reader()?;
        let reader_parser = Arc::clone(&parser);
        let reader_dirty = Arc::clone(&dirty);
        thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if let Ok(mut parser) = reader_parser.lock() {
                            parser.process(&buffer[..read]);
                            reader_dirty.store(true, Ordering::Release);
                        } else {
                            break;
                        }
                    }
                }
            }
        });
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let writer = pair.master.take_writer()?;

        Ok(Self {
            master: pair.master,
            writer,
            child,
            parser,
            dirty,
            exited: None,
        })
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let _ = self.master.resize(size);
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        self.dirty.store(true, Ordering::Release);
    }

    fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    fn poll(&mut self) -> bool {
        if self.exited.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exited = Some(status.exit_code());
            return true;
        }
        false
    }

    fn mouse_scroll(&mut self, up: bool, column: u16, row: u16, modifiers: KeyModifiers) -> bool {
        let encoding = {
            let Ok(parser) = self.parser.lock() else {
                return false;
            };
            let screen = parser.screen();
            if screen.mouse_protocol_mode() == vt100::MouseProtocolMode::None {
                return false;
            }
            screen.mouse_protocol_encoding()
        };
        if let Some(bytes) = encode_mouse_scroll(up, column, row, modifiers, encoding) {
            self.write(&bytes);
            true
        } else {
            false
        }
    }

    fn scroll_scrollback(&mut self, up: bool, rows: usize) {
        if let Ok(mut parser) = self.parser.lock()
            && scroll_parser_scrollback(&mut parser, up, rows)
        {
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn reset_scrollback(&mut self) {
        if let Ok(mut parser) = self.parser.lock()
            && reset_parser_scrollback(&mut parser)
        {
            self.dirty.store(true, Ordering::Release);
        }
    }

    fn scroll_offset(&self) -> usize {
        self.parser
            .lock()
            .map_or(0, |parser| parser.screen().scrollback())
    }

    fn alternate_screen(&self) -> bool {
        self.parser
            .lock()
            .is_ok_and(|parser| parser.screen().alternate_screen())
    }

    fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    fn terminate(&mut self) -> Result<()> {
        self.write(b"\x03");
        thread::sleep(Duration::from_millis(150));
        if self.child.try_wait()?.is_none() {
            let _ = self.child.kill();
        }
        Ok(())
    }
}

impl StatusMonitor {
    fn new(agent: AgentKind, container: String, workdir: String) -> Self {
        let summary = if agent.command().is_some() {
            String::new()
        } else {
            "usage unavailable for custom agent".into()
        };
        Self {
            agent,
            container,
            workdir,
            terminal: None,
            phase: StatusPhase::Idle,
            next_refresh: Instant::now() + STATUS_INITIAL_DELAY,
            has_capture: false,
            retry_attempted: false,
            percent_left: None,
            summary,
            detail: String::new(),
        }
    }

    fn request_refresh(&mut self) {
        if matches!(self.phase, StatusPhase::Idle) {
            self.next_refresh = Instant::now();
        }
    }

    fn poll(&mut self) -> bool {
        if self.agent.command().is_none() {
            return false;
        }

        let now = Instant::now();
        if self.terminal.is_none() {
            if now < self.next_refresh {
                return false;
            }
            let command = status_process_command(&self.container, &self.workdir, self.agent);
            match PtyProcess::spawn(command, 40, 100) {
                Ok(terminal) => {
                    self.terminal = Some(terminal);
                    self.phase = StatusPhase::WaitingForReady(now);
                    return true;
                }
                Err(error) => {
                    self.summary = format!("usage checker unavailable: {error}");
                    self.detail = format!("{error:#}");
                    self.next_refresh = now + STATUS_POLL_INTERVAL;
                    return true;
                }
            }
        }

        let exited = self
            .terminal
            .as_mut()
            .is_some_and(|terminal| terminal.poll() && terminal.exited.is_some());
        if exited {
            let code = self
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.exited)
                .unwrap_or(1);
            let contents = self.contents();
            self.summary = format!("usage checker exited with code {code}");
            self.detail = status_detail(&contents);
            self.terminal = None;
            self.phase = StatusPhase::Idle;
            self.next_refresh = now + STATUS_POLL_INTERVAL;
            return true;
        }

        match self.phase {
            StatusPhase::Idle if now >= self.next_refresh => {
                self.retry_attempted = false;
                if self.has_capture {
                    self.write(b"\x1b");
                    self.phase = StatusPhase::Dismissing(now);
                } else {
                    self.submit(now);
                }
                true
            }
            StatusPhase::WaitingForReady(_)
                if status_screen_ready(self.agent, &self.contents()) =>
            {
                self.retry_attempted = false;
                self.submit(now);
                true
            }
            StatusPhase::WaitingForReady(started) if started.elapsed() >= STATUS_READY_TIMEOUT => {
                let contents = self.contents();
                self.summary = "usage checker did not reach an input prompt".into();
                self.detail = status_detail(&contents);
                if let Some(terminal) = &mut self.terminal {
                    let _ = terminal.terminate();
                }
                self.terminal = None;
                self.phase = StatusPhase::Idle;
                self.next_refresh = now + STATUS_POLL_INTERVAL;
                true
            }
            StatusPhase::Dismissing(started) if started.elapsed() >= STATUS_DISMISS_DELAY => {
                self.submit(now);
                true
            }
            StatusPhase::SubmitPending(started)
                if started.elapsed() >= CODEX_STATUS_SUBMIT_DELAY =>
            {
                self.write(b"\r");
                self.phase = StatusPhase::Capturing(now);
                true
            }
            StatusPhase::Capturing(started) if started.elapsed() >= STATUS_CAPTURE_DELAY => {
                let contents = self.contents();
                self.has_capture = true;
                match read_status(&contents, self.retry_attempted) {
                    StatusRead::Success(status) => {
                        self.percent_left = Some(status.percent_left);
                        self.summary = status.summary;
                        self.detail = status_detail(&contents);
                        self.phase = StatusPhase::Idle;
                    }
                    StatusRead::Retry => {
                        self.retry_attempted = true;
                        self.phase = StatusPhase::RetryPending(now);
                    }
                    StatusRead::Failed => {
                        let command = self
                            .agent
                            .command()
                            .expect("known agent has a status command");
                        self.summary = format!("could not read {command} response");
                        self.detail = status_detail(&contents);
                        self.phase = StatusPhase::Idle;
                    }
                }
                true
            }
            StatusPhase::RetryPending(started) if started.elapsed() >= STATUS_RETRY_DELAY => {
                self.write(b"\x1b");
                self.phase = StatusPhase::Dismissing(now);
                true
            }
            _ => false,
        }
    }

    fn submit(&mut self, now: Instant) {
        let command = self
            .agent
            .command()
            .expect("known agent has a status command");
        self.next_refresh = now + STATUS_POLL_INTERVAL;
        if self.agent.status_submit_delay().is_some() {
            self.write(command.as_bytes());
            self.phase = StatusPhase::SubmitPending(now);
        } else {
            self.write(format!("{command}\r").as_bytes());
            self.phase = StatusPhase::Capturing(now);
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Some(terminal) = &mut self.terminal {
            terminal.write(bytes);
        }
    }

    fn contents(&self) -> String {
        self.terminal
            .as_ref()
            .and_then(|terminal| terminal.parser.lock().ok())
            .map(|parser| parser.screen().contents())
            .unwrap_or_default()
    }

    fn terminate(&mut self) -> Result<()> {
        if let Some(terminal) = &mut self.terminal {
            terminal.terminate()?;
        }
        Ok(())
    }
}

impl StatusMonitors {
    fn get(&self, agent: AgentKind) -> Option<&StatusMonitor> {
        match agent {
            AgentKind::Claude => self.claude.as_ref(),
            AgentKind::Codex => self.codex.as_ref(),
            AgentKind::Other => None,
        }
    }

    fn get_mut(&mut self, agent: AgentKind) -> Option<&mut StatusMonitor> {
        match agent {
            AgentKind::Claude => self.claude.as_mut(),
            AgentKind::Codex => self.codex.as_mut(),
            AgentKind::Other => None,
        }
    }

    fn slot_mut(&mut self, agent: AgentKind) -> Option<&mut Option<StatusMonitor>> {
        match agent {
            AgentKind::Claude => Some(&mut self.claude),
            AgentKind::Codex => Some(&mut self.codex),
            AgentKind::Other => None,
        }
    }

    fn ensure(&mut self, agent: AgentKind, container: &str, workdir: &str) {
        let Some(slot) = self.slot_mut(agent) else {
            return;
        };
        if slot.is_none() {
            *slot = Some(StatusMonitor::new(
                agent,
                container.to_owned(),
                workdir.to_owned(),
            ));
        }
    }

    fn poll(&mut self) -> bool {
        self.claude.as_mut().is_some_and(StatusMonitor::poll)
            | self.codex.as_mut().is_some_and(StatusMonitor::poll)
    }

    fn remove_for_container(&mut self, container: &str) {
        for slot in [&mut self.claude, &mut self.codex] {
            if slot
                .as_ref()
                .is_some_and(|monitor| monitor.container == container)
            {
                if let Some(monitor) = slot {
                    let _ = monitor.terminate();
                }
                *slot = None;
            }
        }
    }

    fn terminate(&mut self) {
        for monitor in [&mut self.claude, &mut self.codex].into_iter().flatten() {
            let _ = monitor.terminate();
        }
    }
}

impl Session {
    fn spawn(
        repo: &Path,
        id: String,
        selected_agent: Option<&str>,
        resume: bool,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let repo = project::find_repo_root_from(repo)?;
        let loaded = Config::load(&repo)?;
        let agent_command = selected_agent
            .map(str::to_owned)
            .unwrap_or_else(|| configured_agent_command(&loaded.config));
        let agent = classify_agent(&agent_command, &loaded.config.agent.default);
        let executable = std::env::current_exe().context("failed to locate agentbox executable")?;
        let command = session_command(&executable, &repo, &id, &agent_command, agent, resume);
        let terminal = PtyProcess::spawn(command, rows, cols)
            .with_context(|| format!("failed to start agentbox in {}", repo.display()))?;
        let container = docker::container_name(&repo, Some(&id));
        let workdir = loaded.config.workspace.container_path;
        let name = repo
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository")
            .to_owned();

        Ok(Self {
            id,
            name,
            repo,
            container,
            agent,
            agent_command,
            workdir,
            terminal,
            selection: None,
            activity: AgentActivity::Starting,
            prompt_was_hidden: false,
        })
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.selection = None;
        self.terminal.resize(rows, cols);
    }

    fn write_input(&mut self, bytes: &[u8]) {
        self.selection = None;
        self.terminal.reset_scrollback();
        self.terminal.write(bytes);
        if bytes.contains(&b'\r') || bytes.contains(&b'\n') {
            self.activity = AgentActivity::Working;
            self.prompt_was_hidden = false;
        }
    }

    fn poll(&mut self) -> bool {
        let mut changed = self.terminal.poll();
        if self.terminal.exited.is_none() {
            let contents = self
                .terminal
                .parser
                .lock()
                .map(|parser| parser.screen().contents())
                .unwrap_or_default();
            let prompt_ready = agent_prompt_ready(self.agent, &contents);
            if self.activity == AgentActivity::Working && !prompt_ready {
                self.prompt_was_hidden = true;
            }
            let next = agent_activity(self.agent, self.activity, self.prompt_was_hidden, &contents);
            if next != self.activity {
                self.activity = next;
                changed = true;
            }
        }
        changed
    }

    fn mouse_scroll(&mut self, up: bool, column: u16, row: u16, modifiers: KeyModifiers) {
        match self.agent.scroll_owner() {
            ScrollOwner::Outer => {
                self.terminal
                    .scroll_scrollback(up, SCROLLBACK_ROWS_PER_TICK);
            }
            ScrollOwner::Child => {
                if self.terminal.mouse_scroll(up, column, row, modifiers) {
                    return;
                }
                if self.terminal.alternate_screen() {
                    // Alternate-scroll convention: full-screen apps without a
                    // mouse protocol expect the wheel as arrow keys.
                    let arrow: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
                    self.terminal.write(&arrow.repeat(SCROLLBACK_ROWS_PER_TICK));
                } else {
                    self.terminal
                        .scroll_scrollback(up, SCROLLBACK_ROWS_PER_TICK);
                }
            }
        }
    }

    fn page_scroll(&mut self, up: bool, rows: usize) -> bool {
        if self.terminal.alternate_screen() {
            return false;
        }
        self.terminal.scroll_scrollback(up, rows);
        true
    }

    fn return_to_live(&mut self) -> bool {
        if self.terminal.scroll_offset() == 0 {
            return false;
        }
        self.terminal.reset_scrollback();
        true
    }

    fn begin_selection(&mut self, row: u16, column: u16) {
        self.selection = Some(Selection {
            anchor: (row, column),
            head: (row, column),
        });
    }

    fn drag_selection(&mut self, row: u16, column: u16) -> bool {
        if let Some(selection) = &mut self.selection
            && selection.head != (row, column)
        {
            selection.head = (row, column);
            return true;
        }
        false
    }

    fn finish_selection(&mut self) -> Option<String> {
        let selection = self.selection?;
        if selection.is_empty() {
            self.selection = None;
            return None;
        }
        let parser = self.terminal.parser.lock().ok()?;
        selection_text(&parser, selection)
    }

    fn clear_selection(&mut self) -> bool {
        self.selection.take().is_some()
    }

    fn selection_range(&self) -> Option<((u16, u16), (u16, u16))> {
        self.selection.map(Selection::normalized)
    }

    fn take_dirty(&self) -> bool {
        self.terminal.take_dirty()
    }

    fn terminate(&mut self) -> Result<()> {
        self.terminal.terminate()
    }
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter(stdout: &mut Stdout) -> Result<Self> {
        enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)?;
        Ok(Self { active: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableMouseCapture,
                Show,
                LeaveAlternateScreen,
                ResetColor
            );
        }
    }
}

enum Overlay {
    None,
    Lazygit(PtyProcess),
    NewSession(RepoPicker),
    SelectAgent(AgentPicker),
    Status,
    Help,
    Message { title: String, body: String },
}

struct RepoPicker {
    query: String,
    selected: usize,
    repos: Vec<PathBuf>,
    error: String,
}

struct AgentPicker {
    repo: PathBuf,
    choices: Vec<AgentChoice>,
    selected: usize,
    error: String,
}

struct AgentChoice {
    label: String,
    command: String,
}

struct App {
    sessions: Vec<Session>,
    status_monitors: StatusMonitors,
    selected: usize,
    overlay: Overlay,
    sequence: usize,
    recent_repos: Vec<PathBuf>,
    width: u16,
    height: u16,
    redraw: bool,
}

impl App {
    fn new(width: u16, height: u16) -> Self {
        Self {
            sessions: Vec::new(),
            status_monitors: StatusMonitors::default(),
            selected: 0,
            overlay: Overlay::None,
            sequence: 0,
            recent_repos: load_recent_repos(),
            width,
            height,
            redraw: true,
        }
    }

    fn terminal_size(&self) -> (u16, u16) {
        let sidebar = sidebar_width(self.width);
        (
            self.height.saturating_sub(2).max(2),
            self.width.saturating_sub(sidebar + 1).max(10),
        )
    }

    fn overlay_size(&self) -> (u16, u16) {
        let sidebar = sidebar_width(self.width);
        (
            self.height.max(2),
            self.width.saturating_sub(sidebar + 1).max(10),
        )
    }

    fn add_session(&mut self, path: &Path, agent: &str) -> Result<()> {
        self.add_session_with_id(path, new_session_id(self.sequence + 1), Some(agent), false)
    }

    fn add_session_with_id(
        &mut self,
        path: &Path,
        id: String,
        agent: Option<&str>,
        resume: bool,
    ) -> Result<()> {
        let (rows, cols) = self.terminal_size();
        self.sequence += 1;
        let session = Session::spawn(path, id, agent, resume, rows, cols)?;
        let repo = session.repo.clone();
        self.status_monitors
            .ensure(session.agent, &session.container, &session.workdir);
        self.sessions.push(session);
        self.selected = self.sessions.len() - 1;
        remember_repo(&mut self.recent_repos, repo);
        let _ = save_recent_repos(&self.recent_repos);
        let _ = save_sessions(&self.saved_sessions());
        self.redraw = true;
        Ok(())
    }

    fn saved_sessions(&self) -> Vec<SavedSession> {
        self.sessions
            .iter()
            .map(|session| SavedSession {
                id: session.id.clone(),
                repo: session.repo.clone(),
                agent: Some(session.agent_command.clone()),
            })
            .collect()
    }

    fn close_active_session(&mut self) -> Result<()> {
        if self.sessions.is_empty() {
            return Ok(());
        }
        let mut session = self.sessions.remove(self.selected);
        self.status_monitors
            .remove_for_container(&session.container);
        let _ = session.terminal.terminate();
        remove_container(&session.container)?;
        for candidate in &self.sessions {
            self.status_monitors
                .ensure(candidate.agent, &candidate.container, &candidate.workdir);
        }
        self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
        save_sessions(&self.saved_sessions())?;
        self.redraw = true;
        Ok(())
    }

    fn open_repo_picker(&mut self) {
        self.overlay = Overlay::NewSession(RepoPicker {
            query: String::new(),
            selected: 0,
            repos: repository_candidates(&self.recent_repos),
            error: String::new(),
        });
        self.request_redraw();
    }

    fn open_agent_picker(&mut self, repo: PathBuf) -> Result<()> {
        let repo = project::find_repo_root_from(&repo)?;
        let loaded = Config::load(&repo)?;
        self.overlay = Overlay::SelectAgent(agent_picker(repo, &loaded.config));
        self.request_redraw();
        Ok(())
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        let (rows, cols) = self.terminal_size();
        for session in &mut self.sessions {
            session.resize(rows, cols);
        }
        let (rows, cols) = self.overlay_size();
        if let Overlay::Lazygit(lazygit) = &mut self.overlay {
            lazygit.resize(rows, cols);
        }
        self.redraw = true;
    }

    fn poll(&mut self) {
        let mut changed = false;
        for session in &mut self.sessions {
            changed |= session.poll();
        }
        changed |= self.status_monitors.poll();
        if changed {
            self.redraw = true;
        }
        let lazygit_exited = if let Overlay::Lazygit(lazygit) = &mut self.overlay {
            lazygit.poll();
            lazygit.exited.is_some()
        } else {
            false
        };
        if lazygit_exited {
            self.overlay = Overlay::None;
            self.redraw = true;
        }
    }

    fn active_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.selected)
    }

    fn move_session_selection(&mut self, direction: SessionDirection) {
        self.selected = session_selection(self.selected, self.sessions.len(), direction);
        self.request_redraw();
    }

    fn active_status(&self) -> Option<&StatusMonitor> {
        let agent = self.sessions.get(self.selected)?.agent;
        self.status_monitors.get(agent)
    }

    fn refresh_active_status(&mut self) {
        let Some(agent) = self
            .sessions
            .get(self.selected)
            .map(|session| session.agent)
        else {
            return;
        };
        if let Some(status) = self.status_monitors.get_mut(agent) {
            status.request_refresh();
        }
    }

    fn request_redraw(&mut self) {
        self.redraw = true;
    }

    fn take_redraw(&mut self) -> bool {
        let terminal_dirty = self
            .sessions
            .get(self.selected)
            .is_some_and(Session::take_dirty);
        let overlay_dirty = match &self.overlay {
            Overlay::Lazygit(lazygit) => lazygit.take_dirty(),
            _ => false,
        };
        let redraw = self.redraw || terminal_dirty || overlay_dirty;
        self.redraw = false;
        redraw
    }

    fn open_lazygit(&mut self) -> Result<()> {
        let repo = self
            .sessions
            .get(self.selected)
            .map(|session| session.repo.clone())
            .context("no active repository")?;
        let (rows, cols) = self.overlay_size();
        let mut command = lazygit_command(&repo);
        command.env("TERM", "xterm-256color");
        let lazygit = PtyProcess::spawn(command, rows, cols)
            .with_context(|| format!("failed to start host lazygit in {}", repo.display()))?;
        self.overlay = Overlay::Lazygit(lazygit);
        self.request_redraw();
        Ok(())
    }

    fn paste_clipboard_image(&mut self) {
        let result = self
            .active_mut()
            .context("no active session")
            .and_then(paste_clipboard_image);
        if let Err(error) = result {
            self.overlay = Overlay::Message {
                title: "Paste screenshot".into(),
                body: format!("{error:#}"),
            };
        }
        self.request_redraw();
    }
}

pub fn run() -> Result<u8> {
    let mut stdout = io::stdout();
    let (width, height) = terminal::size()?;
    let mut app = App::new(width, height);
    cleanup_orphan_containers();
    let saved = load_sessions();
    for session in saved {
        let _ = app.add_session_with_id(&session.repo, session.id, session.agent.as_deref(), true);
    }
    let _ = save_sessions(&app.saved_sessions());
    if app.sessions.is_empty() {
        app.open_repo_picker();
    }
    let _guard = TerminalGuard::enter(&mut stdout)?;

    loop {
        app.poll();
        if app.take_redraw() {
            draw(&mut stdout, &app)?;
        }
        if event::poll(FRAME_INTERVAL)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if matches!(&app.overlay, Overlay::None) && matches!(key.code, KeyCode::F(3)) {
                        if let Err(error) = app.open_lazygit() {
                            app.overlay = Overlay::Message {
                                title: "Lazygit".into(),
                                body: format!("{error:#}"),
                            };
                        }
                        app.request_redraw();
                    } else if handle_key(&mut app, key)? {
                        break;
                    }
                }
                Event::Paste(text) => {
                    if let Overlay::NewSession(picker) = &mut app.overlay {
                        picker.query.push_str(&text);
                        picker.selected = 0;
                        picker.error.clear();
                        app.request_redraw();
                    } else if let Overlay::Lazygit(lazygit) = &mut app.overlay {
                        lazygit.write(text.as_bytes());
                    } else if let Some(session) = app.active_mut() {
                        session.write_input(text.as_bytes());
                    }
                }
                Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
                Event::Resize(width, height) => app.resize(width, height),
                _ => {}
            }
        }
    }

    app.status_monitors.terminate();
    for session in &mut app.sessions {
        let _ = session.terminate();
        let _ = remove_container(&session.container);
    }
    if let Overlay::Lazygit(lazygit) = &mut app.overlay {
        lazygit.terminate()?;
    }
    Ok(0)
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
        return Ok(true);
    }
    let overlay = std::mem::replace(&mut app.overlay, Overlay::None);
    match overlay {
        Overlay::Lazygit(mut lazygit) => {
            if let Some(bytes) = encode_key(key) {
                lazygit.write(&bytes);
            }
            app.overlay = Overlay::Lazygit(lazygit);
            return Ok(false);
        }
        Overlay::NewSession(mut picker) => {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let matches = filtered_repositories(&picker.repos, &picker.query);
                    let path = picker_path(&picker, &matches);
                    match app.open_agent_picker(path) {
                        Ok(()) => {}
                        Err(failure) => {
                            picker.error = format!("{failure:#}");
                            app.overlay = Overlay::NewSession(picker);
                            app.request_redraw();
                        }
                    }
                }
                KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1);
                    app.overlay = Overlay::NewSession(picker);
                    app.request_redraw();
                }
                KeyCode::Down => {
                    let count = filtered_repositories(&picker.repos, &picker.query).len();
                    picker.selected = (picker.selected + 1).min(count.saturating_sub(1));
                    app.overlay = Overlay::NewSession(picker);
                    app.request_redraw();
                }
                KeyCode::Backspace => {
                    picker.query.pop();
                    picker.selected = 0;
                    picker.error.clear();
                    app.overlay = Overlay::NewSession(picker);
                    app.request_redraw();
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    picker.query.push(character);
                    picker.selected = 0;
                    picker.error.clear();
                    app.overlay = Overlay::NewSession(picker);
                    app.request_redraw();
                }
                _ => app.overlay = Overlay::NewSession(picker),
            }
            app.request_redraw();
            return Ok(false);
        }
        Overlay::SelectAgent(mut picker) => {
            match key.code {
                KeyCode::Esc => app.open_repo_picker(),
                KeyCode::Enter => {
                    let command = picker.choices[picker.selected].command.clone();
                    match app.add_session(&picker.repo, &command) {
                        Ok(()) => {}
                        Err(failure) => {
                            picker.error = format!("{failure:#}");
                            app.overlay = Overlay::SelectAgent(picker);
                            app.request_redraw();
                        }
                    }
                }
                KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1);
                    app.overlay = Overlay::SelectAgent(picker);
                }
                KeyCode::Down => {
                    picker.selected =
                        (picker.selected + 1).min(picker.choices.len().saturating_sub(1));
                    app.overlay = Overlay::SelectAgent(picker);
                }
                _ => app.overlay = Overlay::SelectAgent(picker),
            }
            app.request_redraw();
            return Ok(false);
        }
        Overlay::Status | Overlay::Help | Overlay::Message { .. } => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::F(2)
            ) {
                app.request_redraw();
                return Ok(false);
            }
            app.overlay = overlay;
            return Ok(false);
        }
        Overlay::None => {}
    }

    {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('n')) {
            app.open_repo_picker();
            return Ok(false);
        }
        if let Some(direction) = session_direction(key) {
            app.move_session_selection(direction);
            return Ok(false);
        }
        if is_close_session_key(key) {
            if let Err(error) = app.close_active_session() {
                app.overlay = Overlay::Message {
                    title: "Close session".into(),
                    body: format!("{error:#}"),
                };
            } else if app.sessions.is_empty() {
                app.open_repo_picker();
            }
            app.request_redraw();
            return Ok(false);
        }
        if is_paste_image_key(key) {
            app.paste_clipboard_image();
            return Ok(false);
        }
        match key.code {
            KeyCode::F(1) => {
                app.overlay = Overlay::Help;
                app.request_redraw();
            }
            KeyCode::F(2) => {
                app.overlay = Overlay::Status;
                app.request_redraw();
            }
            KeyCode::F(5) => {
                app.refresh_active_status();
                app.request_redraw();
            }
            KeyCode::F(6) if !app.sessions.is_empty() => {
                app.selected = (app.selected + 1) % app.sessions.len();
                app.request_redraw();
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let rows = app.terminal_size().0.saturating_sub(1).max(1) as usize;
                let up = matches!(key.code, KeyCode::PageUp);
                let handled = app
                    .active_mut()
                    .is_some_and(|session| session.page_scroll(up, rows));
                if !handled
                    && let Some(bytes) = encode_key(key)
                    && let Some(session) = app.active_mut()
                {
                    session.write_input(&bytes);
                }
            }
            KeyCode::End => {
                let handled = app.active_mut().is_some_and(Session::return_to_live);
                if !handled
                    && let Some(bytes) = encode_key(key)
                    && let Some(session) = app.active_mut()
                {
                    session.write_input(&bytes);
                }
            }
            _ => {
                if let Some(bytes) = encode_key(key)
                    && let Some(session) = app.active_mut()
                {
                    session.write_input(&bytes);
                }
            }
        }
    }
    Ok(false)
}

fn is_close_session_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('w'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionDirection {
    Previous,
    Next,
}

fn session_direction(key: KeyEvent) -> Option<SessionDirection> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Char('j') => Some(SessionDirection::Next),
        KeyCode::Char('k') => Some(SessionDirection::Previous),
        _ => None,
    }
}

fn session_selection(selected: usize, session_count: usize, direction: SessionDirection) -> usize {
    match direction {
        SessionDirection::Previous => selected.saturating_sub(1),
        SessionDirection::Next => selected
            .saturating_add(1)
            .min(session_count.saturating_sub(1)),
    }
}

fn is_paste_image_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::F(4))
        || (key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('v')))
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    let sidebar = sidebar_width(app.width);
    let terminal_left = sidebar + 1;
    if let Overlay::Lazygit(lazygit) = &mut app.overlay {
        if let Some(up) = scroll_direction(mouse.kind)
            && mouse.column >= terminal_left
        {
            lazygit.mouse_scroll(up, mouse.column - terminal_left, mouse.row, mouse.modifiers);
        }
        return;
    }
    if !matches!(&app.overlay, Overlay::None) {
        return;
    }
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        && mouse.column < sidebar
        && let Some(selected) =
            sidebar_session_at(app.sessions.len(), app.selected, app.height, mouse.row)
    {
        let selection_cleared = app.active_mut().is_some_and(Session::clear_selection);
        if app.selected != selected || selection_cleared {
            app.selected = selected;
            app.request_redraw();
        }
        return;
    }
    let in_chat = mouse.column >= terminal_left && mouse.row < app.height.saturating_sub(2);
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            if !in_chat {
                return;
            }
            let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
            let column = mouse.column - terminal_left;
            let mut cleared = false;
            if let Some(session) = app.active_mut() {
                cleared = session.clear_selection();
                session.mouse_scroll(up, column, mouse.row, mouse.modifiers);
            }
            if cleared {
                app.request_redraw();
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let (row, column) = chat_position(app, &mouse);
            if let Some(session) = app.active_mut() {
                let redraw = if in_chat {
                    session.begin_selection(row, column);
                    true
                } else {
                    session.clear_selection()
                };
                if redraw {
                    app.request_redraw();
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let (row, column) = chat_position(app, &mouse);
            if app
                .active_mut()
                .is_some_and(|session| session.drag_selection(row, column))
            {
                app.request_redraw();
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let (row, column) = chat_position(app, &mouse);
            if let Some(session) = app.active_mut() {
                session.drag_selection(row, column);
                if let Some(text) = session.finish_selection() {
                    copy_to_clipboard(&text);
                }
                app.request_redraw();
            }
        }
        _ => {}
    }
}

fn scroll_direction(kind: MouseEventKind) -> Option<bool> {
    match kind {
        MouseEventKind::ScrollUp => Some(true),
        MouseEventKind::ScrollDown => Some(false),
        _ => None,
    }
}

/// Clamp a mouse position into the chat pane and convert it to pane-relative
/// row/column coordinates.
fn chat_position(app: &App, mouse: &MouseEvent) -> (u16, u16) {
    let terminal_left = sidebar_width(app.width) + 1;
    let right = app.width.saturating_sub(1).max(terminal_left);
    let row = mouse.row.min(app.height.saturating_sub(3));
    let column = mouse.column.clamp(terminal_left, right) - terminal_left;
    (row, column)
}

fn copy_to_clipboard(text: &str) {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(osc52_copy_sequence(text).as_bytes());
    let _ = stdout.flush();
}

fn draw(stdout: &mut Stdout, app: &App) -> Result<()> {
    stdout.sync_update(|stdout| -> Result<()> {
        let sidebar = sidebar_width(app.width);
        queue!(stdout, Hide)?;
        draw_sidebar(stdout, app, sidebar)?;
        draw_terminal(stdout, app, sidebar)?;
        draw_footer(stdout, app, sidebar)?;
        match &app.overlay {
            Overlay::Lazygit(lazygit) => draw_lazygit(stdout, app, lazygit, sidebar)?,
            Overlay::NewSession(picker) => draw_new_session(stdout, app, picker)?,
            Overlay::SelectAgent(picker) => draw_agent_picker(stdout, app, picker)?,
            Overlay::Status => draw_status(stdout, app)?,
            Overlay::Help => draw_help(stdout, app)?,
            Overlay::Message { title, body } => draw_message(stdout, app, title, body)?,
            Overlay::None => draw_cursor(stdout, app, sidebar)?,
        }
        Ok(())
    })??;
    Ok(())
}

fn draw_sidebar(stdout: &mut Stdout, app: &App, width: u16) -> Result<()> {
    let blank = " ".repeat(width as usize);
    for y in 0..app.height {
        queue!(
            stdout,
            MoveTo(0, y),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Print(&blank),
            MoveTo(width, y),
            SetForegroundColor(Color::DarkGrey),
            Print("│")
        )?;
    }
    queue!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        SetAttribute(Attribute::Bold),
        MoveTo(1, 0),
        Print("AGENTBOX"),
        SetAttribute(Attribute::Reset)
    )?;
    let (first, visible) = sidebar_session_window(app.sessions.len(), app.selected, app.height);
    for (index, session) in app.sessions.iter().enumerate().skip(first).take(visible) {
        let y = SIDEBAR_HEADER_ROWS + (index - first) as u16 * SIDEBAR_SESSION_ROWS;
        let selected = index == app.selected;
        queue!(
            stdout,
            MoveTo(1, y),
            SetForegroundColor(if selected { Color::Cyan } else { Color::Grey }),
            SetAttribute(if selected {
                Attribute::Bold
            } else {
                Attribute::Reset
            }),
            Print(if selected { "> " } else { "  " }),
            Print(truncate(&session.name, width.saturating_sub(4) as usize)),
            SetAttribute(Attribute::Reset)
        )?;
        let process_state = session
            .terminal
            .exited
            .map(|code| format!("exited {code}"))
            .unwrap_or_else(|| session.activity.label().into());
        let agent_state = format!("{} · {process_state}", session.agent.label());
        let repo = display_repo_path(&session.repo);
        let status = match session.agent {
            AgentKind::Other => "usage unavailable for custom agent".to_owned(),
            _ => app
                .status_monitors
                .get(session.agent)
                .filter(|status| !status.summary.is_empty())
                .map(|status| status.summary.clone())
                .unwrap_or_else(|| "usage pending".to_owned()),
        };
        queue!(
            stdout,
            MoveTo(3, y + 1),
            SetForegroundColor(if selected {
                Color::Grey
            } else {
                Color::DarkGrey
            }),
            Print(truncate(&agent_state, width.saturating_sub(5) as usize)),
            MoveTo(3, y + 2),
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(&repo, width.saturating_sub(5) as usize)),
            MoveTo(3, y + 3),
            Print(truncate(&status, width.saturating_sub(5) as usize))
        )?;
    }
    Ok(())
}

fn draw_terminal(stdout: &mut Stdout, app: &App, sidebar: u16) -> Result<()> {
    let Some(session) = app.sessions.get(app.selected) else {
        return Ok(());
    };
    let terminal_width = app.width.saturating_sub(sidebar + 1);
    let terminal_height = app.height.saturating_sub(2);
    draw_pty(
        stdout,
        &session.terminal,
        sidebar + 1,
        terminal_width,
        terminal_height,
        session.selection_range(),
    )
}

fn draw_lazygit(stdout: &mut Stdout, app: &App, lazygit: &PtyProcess, sidebar: u16) -> Result<()> {
    let width = app.width.saturating_sub(sidebar + 1);
    draw_pty(stdout, lazygit, sidebar + 1, width, app.height, None)?;
    draw_pty_cursor(stdout, lazygit, sidebar + 1)
}

fn draw_pty(
    stdout: &mut Stdout,
    terminal: &PtyProcess,
    x: u16,
    width: u16,
    height: u16,
    selection: Option<((u16, u16), (u16, u16))>,
) -> Result<()> {
    let parser = terminal
        .parser
        .lock()
        .map_err(|_| anyhow::anyhow!("terminal parser lock was poisoned"))?;
    let screen = parser.screen();
    for row in 0..height {
        queue!(
            stdout,
            MoveTo(x, row),
            ResetColor,
            SetAttribute(Attribute::Reset)
        )?;
        let mut style = None;
        for column in 0..width {
            let Some(cell) = screen.cell(row, column) else {
                break;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut cell_style = TerminalStyle::from(cell);
            if selection.is_some_and(|(start, end)| (row, column) >= start && (row, column) <= end)
            {
                cell_style.inverse = !cell_style.inverse;
            }
            if style != Some(cell_style) {
                cell_style.queue(stdout)?;
                style = Some(cell_style);
            }
            if cell.has_contents() {
                queue!(stdout, Print(cell.contents()))?;
            } else {
                queue!(stdout, Print(" "))?;
            }
        }
    }
    queue!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalStyle {
    foreground: Color,
    background: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl From<&vt100::Cell> for TerminalStyle {
    fn from(cell: &vt100::Cell) -> Self {
        Self {
            foreground: terminal_color(cell.fgcolor()),
            background: terminal_color(cell.bgcolor()),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }
}

impl TerminalStyle {
    fn queue(self, stdout: &mut Stdout) -> Result<()> {
        queue!(
            stdout,
            SetAttribute(Attribute::Reset),
            SetForegroundColor(self.foreground),
            SetBackgroundColor(self.background)
        )?;
        for (enabled, attribute) in [
            (self.bold, Attribute::Bold),
            (self.dim, Attribute::Dim),
            (self.italic, Attribute::Italic),
            (self.underline, Attribute::Underlined),
            (self.inverse, Attribute::Reverse),
        ] {
            if enabled {
                queue!(stdout, SetAttribute(attribute))?;
            }
        }
        Ok(())
    }
}

fn terminal_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::AnsiValue(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb {
            r: red,
            g: green,
            b: blue,
        },
    }
}

fn draw_footer(stdout: &mut Stdout, app: &App, sidebar: u16) -> Result<()> {
    let y = app.height.saturating_sub(2);
    let width = app.width.saturating_sub(sidebar + 1);
    let blank = " ".repeat(width as usize);
    let session = app.sessions.get(app.selected);
    let scrolled = session.map_or(0, |session| session.terminal.scroll_offset());
    let (status, color) = if scrolled > 0 {
        (
            format!("scrolled back {scrolled} lines — wheel down or End to follow"),
            Color::Yellow,
        )
    } else {
        (
            match session {
                Some(session) if session.agent == AgentKind::Other => {
                    "usage unavailable for custom agent".into()
                }
                Some(_) => app
                    .active_status()
                    .map(|status| {
                        usage_footer(
                            &status.summary,
                            status.percent_left,
                            app.width.saturating_sub(sidebar + 3) as usize,
                        )
                    })
                    .unwrap_or_else(|| "usage pending".into()),
                None => "no session".into(),
            },
            Color::DarkGrey,
        )
    };
    queue!(
        stdout,
        MoveTo(sidebar + 1, y),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Print(&blank),
        MoveTo(sidebar + 1, y + 1),
        Print(&blank),
        MoveTo(sidebar + 2, y),
        SetForegroundColor(color),
        Print(truncate(
            &status,
            app.width.saturating_sub(sidebar + 3) as usize
        )),
        MoveTo(sidebar + 2, y + 1),
        SetForegroundColor(Color::DarkGrey),
        Print(truncate(
            "Drag copy  Wheel/PgUp scroll  F1 help  F2 details  F3 lazygit  F5 usage  Ctrl-J/K sessions  Ctrl-N new  Ctrl-C close  Ctrl-Q quit",
            app.width.saturating_sub(sidebar + 3) as usize
        )),
        ResetColor
    )?;
    Ok(())
}

fn draw_cursor(stdout: &mut Stdout, app: &App, sidebar: u16) -> Result<()> {
    let Some(session) = app.sessions.get(app.selected) else {
        return Ok(());
    };
    draw_pty_cursor(stdout, &session.terminal, sidebar + 1)
}

fn draw_pty_cursor(stdout: &mut Stdout, terminal: &PtyProcess, x: u16) -> Result<()> {
    let parser = terminal
        .parser
        .lock()
        .map_err(|_| anyhow::anyhow!("terminal parser lock was poisoned"))?;
    if parser.screen().scrollback() == 0 && !parser.screen().hide_cursor() {
        let (row, column) = parser.screen().cursor_position();
        queue!(stdout, MoveTo(x + column, row), Show)?;
    }
    Ok(())
}

fn draw_new_session(stdout: &mut Stdout, app: &App, picker: &RepoPicker) -> Result<()> {
    let width = app.width.saturating_sub(8).clamp(30, 88);
    let matches = filtered_repositories(&picker.repos, &picker.query);
    let visible_rows = app.height.saturating_sub(10).clamp(3, 10);
    let height = visible_rows + 7;
    let x = (app.width.saturating_sub(width)) / 2;
    let y = (app.height.saturating_sub(height)) / 2;
    draw_box(stdout, x, y, width, height, "Open repository")?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 2),
        SetForegroundColor(Color::Grey),
        Print("Search by repository name or path:"),
        MoveTo(x + 2, y + 3),
        SetForegroundColor(Color::White),
        Print(truncate(&picker.query, width.saturating_sub(4) as usize))
    )?;
    let first_visible = picker
        .selected
        .saturating_sub(visible_rows.saturating_sub(1) as usize);
    for (row, candidate) in matches
        .iter()
        .skip(first_visible)
        .take(visible_rows as usize)
        .enumerate()
    {
        let selected = first_visible + row == picker.selected;
        let marker = if selected { "> " } else { "  " };
        let label = display_repo_path(candidate.path);
        queue!(
            stdout,
            MoveTo(x + 2, y + 5 + row as u16),
            SetForegroundColor(if selected { Color::Cyan } else { Color::Grey }),
            SetAttribute(if selected {
                Attribute::Bold
            } else {
                Attribute::Reset
            }),
            Print(marker),
            Print(truncate(&label, width.saturating_sub(6) as usize)),
            SetAttribute(Attribute::Reset)
        )?;
    }
    let hint = if picker.error.is_empty() {
        if matches.is_empty() {
            "No match; Enter opens the typed path"
        } else {
            "Up/Down select, Enter open, Esc cancel"
        }
    } else {
        &picker.error
    };
    queue!(
        stdout,
        MoveTo(x + 2, y + height.saturating_sub(2)),
        SetForegroundColor(if picker.error.is_empty() {
            Color::DarkGrey
        } else {
            Color::Red
        }),
        Print(truncate(hint, width.saturating_sub(4) as usize)),
        MoveTo(
            x + 2
                + picker
                    .query
                    .chars()
                    .count()
                    .min(width.saturating_sub(4) as usize) as u16,
            y + 3
        ),
        Show
    )?;
    Ok(())
}

fn draw_agent_picker(stdout: &mut Stdout, app: &App, picker: &AgentPicker) -> Result<()> {
    let width = app.width.saturating_sub(8).clamp(30, 72);
    let height = (picker.choices.len() as u16 + 7).min(app.height.saturating_sub(2));
    let x = (app.width.saturating_sub(width)) / 2;
    let y = (app.height.saturating_sub(height)) / 2;
    draw_box(stdout, x, y, width, height, "Choose agent")?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 2),
        SetForegroundColor(Color::Grey),
        Print(truncate(
            &display_repo_path(&picker.repo),
            width.saturating_sub(4) as usize
        ))
    )?;
    for (index, choice) in picker.choices.iter().enumerate() {
        let selected = index == picker.selected;
        queue!(
            stdout,
            MoveTo(x + 2, y + 4 + index as u16),
            SetForegroundColor(if selected { Color::Cyan } else { Color::Grey }),
            SetAttribute(if selected {
                Attribute::Bold
            } else {
                Attribute::Reset
            }),
            Print(if selected { "> " } else { "  " }),
            Print(truncate(&choice.label, width.saturating_sub(6) as usize)),
            SetAttribute(Attribute::Reset)
        )?;
    }
    let hint = if picker.error.is_empty() {
        "Up/Down select, Enter start, Esc back"
    } else {
        &picker.error
    };
    queue!(
        stdout,
        MoveTo(x + 2, y + height.saturating_sub(2)),
        SetForegroundColor(if picker.error.is_empty() {
            Color::DarkGrey
        } else {
            Color::Red
        }),
        Print(truncate(hint, width.saturating_sub(4) as usize))
    )?;
    Ok(())
}

fn draw_status(stdout: &mut Stdout, app: &App) -> Result<()> {
    let width = app.width.saturating_sub(8).clamp(20, 88);
    let height = app.height.saturating_sub(6).clamp(8, 28);
    let x = (app.width.saturating_sub(width)) / 2;
    let y = (app.height.saturating_sub(height)) / 2;
    draw_box(stdout, x, y, width, height, "Usage / status")?;
    let detail = app
        .active_status()
        .map(|status| status.detail.as_str())
        .filter(|detail| !detail.is_empty())
        .unwrap_or("No captured usage/status yet.");
    for (index, line) in detail
        .lines()
        .take(height.saturating_sub(3) as usize)
        .enumerate()
    {
        queue!(
            stdout,
            MoveTo(x + 2, y + 2 + index as u16),
            SetForegroundColor(Color::Grey),
            Print(truncate(line, width.saturating_sub(4) as usize))
        )?;
    }
    Ok(())
}

fn draw_help(stdout: &mut Stdout, app: &App) -> Result<()> {
    let width = app.width.saturating_sub(8).clamp(20, 66);
    let height = 21.min(app.height.saturating_sub(2));
    let x = (app.width.saturating_sub(width)) / 2;
    let y = (app.height.saturating_sub(height)) / 2;
    draw_box(stdout, x, y, width, height, "Keys")?;
    let lines = [
        "Wheel   scroll the active conversation",
        "PgUp/Dn page through the conversation history",
        "End     jump back to the live view",
        "Drag    select text and copy it to the clipboard",
        "F4      paste a clipboard screenshot into the prompt",
        "Ctrl-V  also paste a screenshot when the terminal passes it through",
        "Ctrl-N  open a session in another repository",
        "Ctrl-C  close the active session and remove its container",
        "Ctrl-W  also close the active session",
        "Ctrl-J  select the next session",
        "Ctrl-K  select the previous session",
        "F6      switch to the next session",
        "F3      open host lazygit in the active repository",
        "F5      refresh usage/status now",
        "F2      show the last captured usage/status view",
        "Ctrl-Q  quit; saved sessions resume next launch",
        "",
        "All other keys are sent to the active agent.",
        "Esc closes dialogs.",
    ];
    for (index, line) in lines.iter().enumerate() {
        queue!(
            stdout,
            MoveTo(x + 2, y + 2 + index as u16),
            SetForegroundColor(Color::Grey),
            Print(truncate(line, width.saturating_sub(4) as usize))
        )?;
    }
    Ok(())
}

fn draw_message(stdout: &mut Stdout, app: &App, title: &str, body: &str) -> Result<()> {
    let width = app.width.saturating_sub(8).clamp(20, 76);
    let body_lines = body.lines().count().clamp(1, 6) as u16;
    let height = (body_lines + 5).min(app.height.saturating_sub(2));
    let x = (app.width.saturating_sub(width)) / 2;
    let y = (app.height.saturating_sub(height)) / 2;
    draw_box(stdout, x, y, width, height, title)?;
    for (index, line) in body
        .lines()
        .take(height.saturating_sub(4) as usize)
        .enumerate()
    {
        queue!(
            stdout,
            MoveTo(x + 2, y + 2 + index as u16),
            SetForegroundColor(Color::Red),
            Print(truncate(line, width.saturating_sub(4) as usize))
        )?;
    }
    queue!(
        stdout,
        MoveTo(x + 2, y + height.saturating_sub(2)),
        SetForegroundColor(Color::DarkGrey),
        Print("Esc or Enter to close")
    )?;
    Ok(())
}

fn lazygit_command(repo: &Path) -> CommandBuilder {
    let mut command = CommandBuilder::new("lazygit");
    command.cwd(repo);
    command
}

fn session_command(
    executable: &Path,
    repo: &Path,
    session_id: &str,
    agent_command: &str,
    agent: AgentKind,
    resume: bool,
) -> CommandBuilder {
    let mut command = CommandBuilder::new(executable);
    command.arg("run");
    command.arg(agent_command);
    match agent {
        AgentKind::Claude if resume => {
            command.arg("--");
            command.arg("--continue");
        }
        AgentKind::Codex => {
            command.arg("--");
            command.arg("--no-alt-screen");
            if resume {
                command.arg("resume");
                command.arg("--last");
            }
        }
        AgentKind::Claude | AgentKind::Other => {}
    }
    command.cwd(repo);
    command.env("TERM", "xterm-256color");
    command.env("AGENTBOX_SESSION_ID", session_id);
    command.env("AGENTBOX_OWNER_PID", process::id().to_string());
    command
}

fn status_process_command(container: &str, workdir: &str, agent: AgentKind) -> CommandBuilder {
    let mut command = CommandBuilder::new("docker");
    command.args([
        "exec",
        "-i",
        "-t",
        "-e",
        "TERM=xterm-256color",
        "-w",
        workdir,
        container,
        "/bin/sh",
        "-c",
        r#"PATH="$HOME/.agentbox/npm/bin:$PATH"; export PATH; exec "$@""#,
        "agentbox-status",
    ]);
    match agent {
        AgentKind::Claude => {
            command.args(["claude", "--dangerously-skip-permissions"]);
        }
        AgentKind::Codex => {
            command.args([
                "codex",
                "--dangerously-bypass-approvals-and-sandbox",
                "--no-alt-screen",
            ]);
        }
        AgentKind::Other => {}
    }
    command
}

fn draw_box(
    stdout: &mut Stdout,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    title: &str,
) -> Result<()> {
    let inner = " ".repeat(width.saturating_sub(2) as usize);
    for row in 0..height {
        queue!(
            stdout,
            MoveTo(x, y + row),
            SetForegroundColor(Color::DarkGrey),
            Print(if row == 0 || row == height - 1 {
                format!("+{}+", "-".repeat(width.saturating_sub(2) as usize))
            } else {
                format!("|{inner}|")
            })
        )?;
    }
    queue!(
        stdout,
        MoveTo(x + 2, y),
        SetForegroundColor(Color::Cyan),
        SetAttribute(Attribute::Bold),
        Print(format!(" {title} ")),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn sidebar_width(total: u16) -> u16 {
    if total < 70 {
        20.min(total / 3)
    } else {
        SIDEBAR_WIDTH
    }
}

fn sidebar_session_window(session_count: usize, selected: usize, height: u16) -> (usize, usize) {
    let available =
        height.saturating_sub(SIDEBAR_HEADER_ROWS + SIDEBAR_FOOTER_ROWS) / SIDEBAR_SESSION_ROWS;
    let visible = usize::from(available).min(session_count);
    if visible == 0 {
        return (0, 0);
    }
    let first = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(session_count - visible);
    (first, visible)
}

fn sidebar_session_at(
    session_count: usize,
    selected: usize,
    height: u16,
    row: u16,
) -> Option<usize> {
    let relative_row = row.checked_sub(SIDEBAR_HEADER_ROWS)?;
    let slot = usize::from(relative_row / SIDEBAR_SESSION_ROWS);
    let (first, visible) = sidebar_session_window(session_count, selected, height);
    (slot < visible).then_some(first + slot)
}

fn classify_agent(command: &str, default: &str) -> AgentKind {
    let executable = if command.trim().is_empty() {
        default
    } else {
        command
    };
    match shell_words::split(executable)
        .ok()
        .and_then(|parts| parts.into_iter().next())
        .as_deref()
    {
        Some("claude") => AgentKind::Claude,
        Some("codex") => AgentKind::Codex,
        _ => AgentKind::Other,
    }
}

fn agent_activity(
    agent: AgentKind,
    current: AgentActivity,
    prompt_was_hidden: bool,
    contents: &str,
) -> AgentActivity {
    if !agent_prompt_ready(agent, contents) {
        return current;
    }
    match current {
        AgentActivity::Starting => AgentActivity::Waiting,
        AgentActivity::Working if prompt_was_hidden && agent_is_asking(contents) => {
            AgentActivity::Waiting
        }
        AgentActivity::Working if prompt_was_hidden => AgentActivity::Done,
        _ => current,
    }
}

fn agent_prompt_ready(agent: AgentKind, contents: &str) -> bool {
    match agent {
        AgentKind::Claude => contents
            .lines()
            .rev()
            .take(4)
            .any(|line| line.contains('❯')),
        AgentKind::Codex => contents
            .lines()
            .rev()
            .take(4)
            .any(|line| line.contains('›')),
        AgentKind::Other => false,
    }
}

fn agent_is_asking(contents: &str) -> bool {
    contents
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.contains('❯') && !line.contains('›'))
        .take(4)
        .any(|line| {
            line.ends_with('?')
                || line.contains("waiting for your response")
                || line.contains("Waiting for your response")
        })
}

fn configured_agent_command(config: &Config) -> String {
    if config.agent.command.trim().is_empty() {
        config.agent.default.clone()
    } else {
        config.agent.command.clone()
    }
}

fn agent_picker(repo: PathBuf, config: &Config) -> AgentPicker {
    let configured = configured_agent_command(config);
    let configured_kind = classify_agent(&configured, &config.agent.default);
    let mut choices = vec![
        AgentChoice {
            label: "Claude".into(),
            command: if configured_kind == AgentKind::Claude {
                configured.clone()
            } else {
                "claude".into()
            },
        },
        AgentChoice {
            label: "Codex".into(),
            command: if configured_kind == AgentKind::Codex {
                configured.clone()
            } else {
                "codex".into()
            },
        },
    ];
    if configured_kind == AgentKind::Other {
        choices.push(AgentChoice {
            label: format!("Configured: {configured}"),
            command: configured,
        });
    }
    let selected = match configured_kind {
        AgentKind::Claude => 0,
        AgentKind::Codex => 1,
        AgentKind::Other => 2,
    };
    AgentPicker {
        repo,
        choices,
        selected,
        error: String::new(),
    }
}

fn parse_usage(contents: &str) -> Option<UsageStatus> {
    let lines = contents
        .lines()
        .map(|line| line.trim().trim_matches('│').trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    if let Some(line) = lines
        .iter()
        .find(|line| is_five_hour_label(line) && usage_percent_left(line).is_some())
    {
        return format_usage_status(usage_percent_left(line)?, reset_time(line));
    }

    if let Some(index) = lines
        .iter()
        .position(|line| line.eq_ignore_ascii_case("current session"))
    {
        let session = lines[index + 1..]
            .iter()
            .take_while(|line| !line.to_ascii_lowercase().starts_with("current week"))
            .copied()
            .collect::<Vec<_>>();
        let percent = session.iter().find_map(|line| usage_percent_left(line))?;
        let reset = session.iter().find_map(|line| reset_time(line));
        return format_usage_status(percent, reset);
    }

    None
}

#[cfg(test)]
fn parse_usage_summary(contents: &str) -> Option<String> {
    parse_usage(contents).map(|status| status.summary)
}

fn read_status(contents: &str, retry_attempted: bool) -> StatusRead {
    match parse_usage(contents) {
        Some(status) => StatusRead::Success(status),
        None if retry_attempted => StatusRead::Failed,
        None => StatusRead::Retry,
    }
}

fn is_five_hour_label(line: &str) -> bool {
    let lowercase = line.to_ascii_lowercase();
    [
        "5h limit",
        "5 hour limit",
        "5-hour limit",
        "five hour limit",
    ]
    .iter()
    .any(|label| lowercase.contains(label))
}

fn usage_percent_left(line: &str) -> Option<u8> {
    let percent = line.find('%')?;
    let start = line[..percent]
        .rfind(|character: char| !character.is_ascii_digit())
        .map_or(0, |index| index + 1);
    let value = line[start..percent].parse::<u8>().ok()?;
    let lowercase = line.to_ascii_lowercase();
    if lowercase[percent..].contains("left") {
        Some(value)
    } else if lowercase[percent..].contains("used") {
        Some(100_u8.saturating_sub(value))
    } else {
        None
    }
}

fn reset_time(line: &str) -> Option<String> {
    let lowercase = line.to_ascii_lowercase();
    let reset = lowercase.find("reset")?;
    line[reset..]
        .split_whitespace()
        .find_map(normalize_clock_time)
}

fn normalize_clock_time(value: &str) -> Option<String> {
    let value =
        value.trim_matches(|character: char| matches!(character, '(' | ')' | ',' | '.' | ';'));
    let lowercase = value.to_ascii_lowercase();
    let (clock, suffix) = if let Some(clock) = lowercase.strip_suffix("am") {
        (clock, Some("am"))
    } else if let Some(clock) = lowercase.strip_suffix("pm") {
        (clock, Some("pm"))
    } else {
        (lowercase.as_str(), None)
    };
    let (hours, minutes) = clock.split_once(':')?;
    let mut hours = hours.parse::<u8>().ok()?;
    let minutes = minutes.parse::<u8>().ok()?;
    if minutes > 59 || suffix.is_some() && !(1..=12).contains(&hours) {
        return None;
    }
    match suffix {
        Some("am") if hours == 12 => hours = 0,
        Some("pm") if hours < 12 => hours += 12,
        None if hours > 23 => return None,
        _ => {}
    }
    Some(format!("{hours:02}:{minutes:02}"))
}

fn format_usage_status(percent_left: u8, reset: Option<String>) -> Option<UsageStatus> {
    let summary = match reset {
        Some(reset) => format!("{percent_left}% left, resets {reset}"),
        None => format!("{percent_left}% left"),
    };
    Some(UsageStatus {
        percent_left,
        summary,
    })
}

fn usage_footer(summary: &str, percent_left: Option<u8>, width: usize) -> String {
    let Some(percent_left) = percent_left else {
        return summary.to_owned();
    };
    let marker = format!("{percent_left}% left");
    let Some(marker_end) = summary.find(&marker).map(|start| start + marker.len()) else {
        return summary.to_owned();
    };
    let reserved = summary.len() + 3;
    let bar_width = width.saturating_sub(reserved).clamp(5, 20);
    let filled = (usize::from(percent_left.min(100)) * bar_width + 50) / 100;
    let bar = format!(
        " [{}{}]",
        "=".repeat(filled),
        "-".repeat(bar_width.saturating_sub(filled))
    );
    format!(
        "{}{}{}",
        &summary[..marker_end],
        bar,
        &summary[marker_end..]
    )
}

fn status_screen_ready(agent: AgentKind, contents: &str) -> bool {
    match agent {
        AgentKind::Claude => contents.contains("Claude Code") && contents.contains('❯'),
        AgentKind::Codex => contents.contains("OpenAI Codex") && contents.contains('›'),
        AgentKind::Other => false,
    }
}

fn status_detail(contents: &str) -> String {
    let lines = contents
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    lines
        .into_iter()
        .rev()
        .take(24)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

fn paste_clipboard_image(session: &mut Session) -> Result<PathBuf> {
    let relative_dir = Path::new(".agentbox/uploads");
    let host_dir = session.repo.join(relative_dir);
    fs::create_dir_all(&host_dir)
        .with_context(|| format!("failed to create {}", host_dir.display()))?;
    ensure_uploads_ignored(&session.repo)?;

    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let filename = format!("screenshot-{millis}.png");
    let host_path = host_dir.join(&filename);
    capture_clipboard_png(&host_path)?;

    let container_path = Path::new(&session.workdir)
        .join(relative_dir)
        .join(filename);
    let prompt_text = format!(" {} ", container_path.display());
    session.write_input(prompt_text.as_bytes());
    Ok(host_path)
}

fn ensure_uploads_ignored(repo: &Path) -> Result<()> {
    let path = repo.join(".agentbox/.gitignore");
    let mut contents = fs::read_to_string(&path).unwrap_or_default();
    if contents.lines().any(|line| line.trim() == "uploads/") {
        return Ok(());
    }
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("uploads/\n");
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn capture_clipboard_png(destination: &Path) -> Result<()> {
    let mut failures = Vec::new();

    #[cfg(target_os = "macos")]
    {
        match Command::new("pngpaste").arg(destination).output() {
            Ok(output) if output.status.success() => return validate_clipboard_png(destination),
            Ok(output) => failures.push(command_failure("pngpaste", &output.stderr)),
            Err(error) => failures.push(format!("pngpaste: {error}")),
        }

        let script = r#"
on run argv
    set outputPath to item 1 of argv
    set imageData to the clipboard as «class PNGf»
    set outputFile to open for access POSIX file outputPath with write permission
    try
        set eof outputFile to 0
        write imageData to outputFile
        close access outputFile
    on error message
        try
            close access outputFile
        end try
        error message
    end try
end run
"#;
        match Command::new("osascript")
            .args(["-e", script, "--"])
            .arg(destination)
            .output()
        {
            Ok(output) if output.status.success() => return validate_clipboard_png(destination),
            Ok(output) => failures.push(command_failure("osascript", &output.stderr)),
            Err(error) => failures.push(format!("osascript: {error}")),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        for (program, args) in [
            ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
            (
                "xclip",
                vec!["-selection", "clipboard", "-t", "image/png", "-o"],
            ),
        ] {
            match Command::new(program).args(&args).output() {
                Ok(output) if output.status.success() => {
                    validate_png_bytes(&output.stdout)?;
                    fs::write(destination, output.stdout).with_context(|| {
                        format!("failed to write screenshot to {}", destination.display())
                    })?;
                    return Ok(());
                }
                Ok(output) => failures.push(command_failure(program, &output.stderr)),
                Err(error) => failures.push(format!("{program}: {error}")),
            }
        }
    }

    anyhow::bail!(
        "could not read a PNG image from the host clipboard. {}",
        failures.join("; ")
    )
}

fn command_failure(program: &str, stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr);
    let message = message.trim();
    if message.is_empty() {
        format!("{program}: clipboard did not contain PNG data")
    } else {
        format!("{program}: {message}")
    }
}

#[cfg(target_os = "macos")]
fn validate_clipboard_png(path: &Path) -> Result<()> {
    let result = fs::read(path)
        .with_context(|| format!("failed to read captured screenshot {}", path.display()))
        .and_then(|bytes| validate_png_bytes(&bytes));
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn validate_png_bytes(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        anyhow::bail!(
            "clipboard image is too large (maximum {} MiB)",
            MAX_CLIPBOARD_IMAGE_BYTES / 1024 / 1024
        );
    }
    if !bytes.starts_with(PNG_SIGNATURE) {
        anyhow::bail!("clipboard does not contain a PNG image");
    }
    Ok(())
}

fn new_session_id(sequence: usize) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("tui-{millis}-{}-{sequence}", process::id())
}

fn sessions_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|directory| directory.join("agentbox/sessions.json"))
}

fn load_sessions() -> Vec<SavedSession> {
    let Some(path) = sessions_path() else {
        return Vec::new();
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Vec<SavedSession>>(&contents).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|session: &SavedSession| session.repo.is_dir())
        .collect()
}

fn save_sessions(sessions: &[SavedSession]) -> Result<()> {
    let path = sessions_path().context("could not determine the user data directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let contents = serde_json::to_string_pretty(sessions)?;
    fs::write(&path, format!("{contents}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn cleanup_orphan_containers() {
    let Ok(output) = process::Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=agentbox.managed=true",
            "--format",
            "{{.Names}}\t{{.Label \"agentbox.owner\"}}",
        ])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.splitn(2, '\t');
        let container = fields.next().unwrap_or_default();
        let owner = fields.next().unwrap_or_default();
        if !container.is_empty() && !owner_process_alive(owner) {
            let _ = remove_container(container);
        }
    }
}

fn owner_process_alive(owner: &str) -> bool {
    !owner.is_empty()
        && owner.parse::<u32>().is_ok()
        && process::Command::new("kill")
            .args(["-0", owner])
            .status()
            .is_ok_and(|status| status.success())
}

fn remove_container(container: &str) -> Result<()> {
    let output = process::Command::new("docker")
        .args(["rm", "-f", container])
        .output()
        .context("failed to execute docker")?;
    if !output.status.success()
        && !String::from_utf8_lossy(&output.stderr).contains("No such container")
    {
        anyhow::bail!(
            "failed to remove container {container}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RepoMatch<'a> {
    path: &'a PathBuf,
    score: i64,
}

fn recent_repos_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|directory| directory.join("agentbox/recent-repos.json"))
}

fn load_recent_repos() -> Vec<PathBuf> {
    let Some(path) = recent_repos_path() else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<PathBuf>>(&contents)
        .unwrap_or_default()
        .into_iter()
        .filter(|path| path.is_dir())
        .take(RECENT_REPO_LIMIT)
        .collect()
}

fn save_recent_repos(repos: &[PathBuf]) -> Result<()> {
    let path = recent_repos_path().context("could not determine the user data directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let contents = serde_json::to_string_pretty(repos)?;
    fs::write(&path, format!("{contents}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn remember_repo(repos: &mut Vec<PathBuf>, repo: PathBuf) {
    repos.retain(|existing| existing != &repo);
    repos.insert(0, repo);
    repos.truncate(RECENT_REPO_LIMIT);
}

fn repository_candidates(recent: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for repo in recent {
        if repo.is_dir() && seen.insert(repo.clone()) {
            candidates.push(repo.clone());
        }
    }

    let mut discovered = Vec::new();
    if let Some(home) = dirs::home_dir() {
        discover_repositories(&home, 0, &mut discovered, &mut seen, DISCOVERED_REPO_LIMIT);
    }
    discovered.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
    candidates.extend(discovered);
    candidates
}

fn discover_repositories(
    directory: &Path,
    depth: usize,
    repos: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    limit: usize,
) {
    if depth > REPO_SEARCH_DEPTH || repos.len() >= limit {
        return;
    }
    if directory.join(".git").exists() {
        if let Ok(repo) = fs::canonicalize(directory)
            && seen.insert(repo.clone())
        {
            repos.push(repo);
        }
        return;
    }

    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if repos.len() >= limit {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !path.is_dir()
            || name.starts_with('.')
            || matches!(
                name.as_ref(),
                "node_modules" | "target" | "vendor" | "Library"
            )
        {
            continue;
        }
        discover_repositories(&path, depth + 1, repos, seen, limit);
    }
}

fn filtered_repositories<'a>(repos: &'a [PathBuf], query: &str) -> Vec<RepoMatch<'a>> {
    let query = query.trim().to_ascii_lowercase();
    let mut matches = repos
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let searchable = path.to_string_lossy().to_ascii_lowercase();
            let score = if query.is_empty() {
                -(index as i64)
            } else {
                fuzzy_score(&searchable, &query)?
            };
            Some(RepoMatch { path, score })
        })
        .collect::<Vec<_>>();
    if !query.is_empty() {
        matches.sort_by(|left, right| {
            right.score.cmp(&left.score).then_with(|| {
                left.path
                    .to_string_lossy()
                    .cmp(&right.path.to_string_lossy())
            })
        });
    }
    matches
}

fn picker_path(picker: &RepoPicker, matches: &[RepoMatch<'_>]) -> PathBuf {
    let typed = expand_path(&picker.query);
    if !picker.query.trim().is_empty() && typed.is_dir() {
        return typed;
    }
    matches
        .get(picker.selected)
        .map(|candidate| candidate.path.clone())
        .unwrap_or(typed)
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    if let Some(index) = candidate.find(query) {
        return Some(10_000 - index as i64 - candidate.len() as i64);
    }

    let mut score = 0_i64;
    let mut query_chars = query.chars();
    let mut wanted = query_chars.next()?;
    let mut previous_match = None;
    for (index, character) in candidate.chars().enumerate() {
        if character != wanted {
            continue;
        }
        score += 100 - index as i64;
        if previous_match.is_some_and(|previous| previous + 1 == index) {
            score += 50;
        }
        previous_match = Some(index);
        match query_chars.next() {
            Some(next) => wanted = next,
            None => return Some(score - candidate.len() as i64),
        }
    }
    None
}

fn display_repo_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = path.strip_prefix(home)
    {
        return format!("~/{}", relative.display());
    }
    path.display().to_string()
}

fn expand_path(input: &str) -> PathBuf {
    let trimmed = input.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(trimmed));
    }
    if let Some(relative) = trimmed.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(relative);
    }
    PathBuf::from(trimmed)
}

fn truncate(value: &str, width: usize) -> String {
    value.chars().take(width).collect()
}

fn selection_text(parser: &vt100::Parser, selection: Selection) -> Option<String> {
    if selection.is_empty() {
        return None;
    }
    let ((start_row, start_col), (end_row, end_col)) = selection.normalized();
    let text =
        parser
            .screen()
            .contents_between(start_row, start_col, end_row, end_col.saturating_add(1));
    (!text.is_empty()).then_some(text)
}

fn osc52_copy_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        encoded.push(ALPHABET[(group >> 18) as usize & 63] as char);
        encoded.push(ALPHABET[(group >> 12) as usize & 63] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(group >> 6) as usize & 63] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[group as usize & 63] as char
        } else {
            '='
        });
    }
    encoded
}

fn scroll_parser_scrollback(parser: &mut vt100::Parser, up: bool, rows: usize) -> bool {
    let current = parser.screen().scrollback();
    let target = if up {
        current.saturating_add(rows)
    } else {
        current.saturating_sub(rows)
    };
    parser.screen_mut().set_scrollback(target);
    parser.screen().scrollback() != current
}

fn reset_parser_scrollback(parser: &mut vt100::Parser) -> bool {
    let current = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(0);
    current != 0
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if alt {
        bytes.push(0x1b);
    }
    match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let lowercase = character.to_ascii_lowercase();
            if lowercase.is_ascii_lowercase() {
                bytes.push(lowercase as u8 - b'a' + 1);
            } else {
                return None;
            }
        }
        KeyCode::Char(character) => {
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        }
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::BackTab => bytes.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        _ => return None,
    }
    Some(bytes)
}

fn encode_mouse_scroll(
    up: bool,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let mut button = if up { 64 } else { 65 };
    if modifiers.contains(KeyModifiers::SHIFT) {
        button += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        button += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        button += 16;
    }
    let column = u32::from(column) + 1;
    let row = u32::from(row) + 1;

    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            Some(format!("\x1b[<{button};{column};{row}M").into_bytes())
        }
        vt100::MouseProtocolEncoding::Default => {
            let button = u8::try_from(button + 32).ok()?;
            let column = u8::try_from(column + 32).ok()?;
            let row = u8::try_from(row + 32).ok()?;
            Some(vec![0x1b, b'[', b'M', button, column, row])
        }
        vt100::MouseProtocolEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            for value in [button + 32, column + 32, row + 32] {
                let character = char::from_u32(value)?;
                let mut encoded = [0_u8; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            Some(bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_configured_agents() {
        assert_eq!(classify_agent("claude --flag", "codex"), AgentKind::Claude);
        assert_eq!(classify_agent("", "codex"), AgentKind::Codex);
        assert_eq!(classify_agent("wrapper claude", "claude"), AgentKind::Other);
    }

    #[test]
    fn agent_activity_tracks_ready_working_waiting_and_done_states() {
        assert_eq!(
            agent_activity(
                AgentKind::Codex,
                AgentActivity::Starting,
                false,
                "OpenAI Codex\n›"
            ),
            AgentActivity::Waiting
        );
        assert_eq!(
            agent_activity(
                AgentKind::Codex,
                AgentActivity::Working,
                true,
                "Implemented the change.\n›"
            ),
            AgentActivity::Done
        );
        assert_eq!(
            agent_activity(
                AgentKind::Claude,
                AgentActivity::Working,
                true,
                "Which database should I use?\n❯"
            ),
            AgentActivity::Waiting
        );
        assert_eq!(
            agent_activity(AgentKind::Codex, AgentActivity::Working, true, "Working..."),
            AgentActivity::Working
        );
        assert_eq!(
            agent_activity(
                AgentKind::Codex,
                AgentActivity::Working,
                false,
                "Old prompt still visible\n›"
            ),
            AgentActivity::Working
        );
    }

    #[test]
    fn sidebar_session_window_keeps_the_selected_session_visible() {
        assert_eq!(sidebar_session_window(6, 0, 19), (0, 3));
        assert_eq!(sidebar_session_window(6, 2, 19), (0, 3));
        assert_eq!(sidebar_session_window(6, 3, 19), (1, 3));
        assert_eq!(sidebar_session_window(6, 5, 19), (3, 3));
    }

    #[test]
    fn sidebar_session_window_handles_short_terminals() {
        assert_eq!(sidebar_session_window(3, 2, 9), (2, 1));
        assert_eq!(sidebar_session_window(3, 2, 4), (0, 0));
        assert_eq!(sidebar_session_window(0, 0, 24), (0, 0));
    }

    #[test]
    fn sidebar_rows_map_to_visible_sessions() {
        assert_eq!(sidebar_session_at(6, 3, 19, 0), None);
        assert_eq!(sidebar_session_at(6, 3, 19, 1), None);
        assert_eq!(sidebar_session_at(6, 3, 19, 2), Some(1));
        assert_eq!(sidebar_session_at(6, 3, 19, 6), Some(1));
        assert_eq!(sidebar_session_at(6, 3, 19, 7), Some(2));
        assert_eq!(sidebar_session_at(6, 3, 19, 16), Some(3));
        assert_eq!(sidebar_session_at(6, 3, 19, 17), None);
    }

    #[test]
    fn sidebar_rows_ignore_sessions_that_are_not_visible() {
        assert_eq!(sidebar_session_at(3, 2, 4, 2), None);
        assert_eq!(sidebar_session_at(0, 0, 24, 2), None);
    }

    #[test]
    fn recent_repositories_are_deduplicated_and_limited() {
        let mut repos = (0..RECENT_REPO_LIMIT)
            .map(|index| PathBuf::from(format!("/repo-{index}")))
            .collect::<Vec<_>>();

        remember_repo(&mut repos, PathBuf::from("/repo-3"));
        assert_eq!(repos[0], PathBuf::from("/repo-3"));
        assert_eq!(repos.len(), RECENT_REPO_LIMIT);

        remember_repo(&mut repos, PathBuf::from("/new-repo"));
        assert_eq!(repos[0], PathBuf::from("/new-repo"));
        assert_eq!(repos.len(), RECENT_REPO_LIMIT);
        assert_eq!(
            repos
                .iter()
                .filter(|repo| *repo == Path::new("/repo-3"))
                .count(),
            1
        );
    }

    #[test]
    fn saved_sessions_round_trip_as_json() {
        let sessions = vec![SavedSession {
            id: "tui-123".into(),
            repo: PathBuf::from("/tmp/project"),
            agent: Some("codex".into()),
        }];

        let json = serde_json::to_string(&sessions).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<SavedSession>>(&json).unwrap(),
            sessions
        );
    }

    #[test]
    fn saved_sessions_without_an_agent_remain_compatible() {
        let session =
            serde_json::from_str::<SavedSession>(r#"{"id":"tui-old","repo":"/tmp/project"}"#)
                .unwrap();

        assert_eq!(session.agent, None);
    }

    #[test]
    fn agent_picker_defaults_to_configured_builtin_agent() {
        let mut config = Config::default();
        config.agent.command = "codex".into();
        let picker = agent_picker(PathBuf::from("/tmp/project"), &config);

        assert_eq!(picker.selected, 1);
        assert_eq!(picker.choices.len(), 2);
        assert_eq!(picker.choices[0].command, "claude");
        assert_eq!(picker.choices[1].command, "codex");
    }

    #[test]
    fn agent_picker_includes_custom_configured_agent() {
        let mut config = Config::default();
        config.agent.command = "my-agent --profile local".into();
        let picker = agent_picker(PathBuf::from("/tmp/project"), &config);

        assert_eq!(picker.selected, 2);
        assert_eq!(picker.choices.len(), 3);
        assert_eq!(picker.choices[2].command, "my-agent --profile local");
    }

    #[test]
    fn current_process_is_not_treated_as_an_orphan_owner() {
        assert!(owner_process_alive(&process::id().to_string()));
        assert!(!owner_process_alive(""));
        assert!(!owner_process_alive("not-a-pid"));
    }

    #[test]
    fn repository_filter_supports_substrings_and_fuzzy_subsequences() {
        let repos = vec![
            PathBuf::from("/home/dev/agentbox"),
            PathBuf::from("/home/dev/customer-api"),
            PathBuf::from("/home/dev/archived-box"),
        ];

        let substring = filtered_repositories(&repos, "agent");
        assert_eq!(substring[0].path, &PathBuf::from("/home/dev/agentbox"));

        let fuzzy = filtered_repositories(&repos, "capi");
        assert_eq!(fuzzy[0].path, &PathBuf::from("/home/dev/customer-api"));
    }

    #[test]
    fn repository_discovery_finds_git_directories_and_stops_at_them() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("code/project");
        let nested = repo.join("nested");
        fs::create_dir_all(nested.join(".git")).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        let mut repos = Vec::new();
        let mut seen = HashSet::new();

        discover_repositories(temp.path(), 0, &mut repos, &mut seen, 10);

        assert_eq!(repos, vec![fs::canonicalize(repo).unwrap()]);
    }

    #[test]
    fn existing_typed_directory_wins_over_a_fuzzy_match() {
        let temp = tempfile::tempdir().unwrap();
        let repos = vec![PathBuf::from("/some/other/repository")];
        let picker = RepoPicker {
            query: temp.path().display().to_string(),
            selected: 0,
            repos,
            error: String::new(),
        };
        let matches = filtered_repositories(&picker.repos, &picker.query);

        assert_eq!(picker_path(&picker, &matches), temp.path());
    }

    #[test]
    fn status_summary_rejects_output_without_usage_value() {
        let status = "header\nModel: codex\nContext remaining: 72%\nprompt";
        assert_eq!(parse_usage_summary(status), None);
    }

    #[test]
    fn incomplete_status_is_retried_only_once() {
        let codex = "5h limit:\nWeekly limit: 84% left";
        let claude = "Current session\nCurrent week (all models)\n55% used";

        assert_eq!(read_status(codex, false), StatusRead::Retry);
        assert_eq!(read_status(codex, true), StatusRead::Failed);
        assert_eq!(read_status(claude, false), StatusRead::Retry);
        assert_eq!(read_status(claude, true), StatusRead::Failed);
    }

    #[test]
    fn status_summary_prefers_codex_five_hour_limit_over_weekly_limit() {
        let status = "\
│  5h limit: [█████████████████░░░] 87% left (resets 19:37) │\n\
│  Weekly limit: [█████████████████░░░] 84% left             │\n\
│  (resets 08:50 on 18 Jun)                                  │";

        assert_eq!(
            parse_usage_summary(status),
            Some("87% left, resets 19:37".into())
        );
    }

    #[test]
    fn status_summary_maps_claude_current_session_to_five_hour_usage() {
        let status = "\
Current session\n\
██████████████████████████████████▌ 69% used\n\
Resets 6:10pm (UTC)\n\
Current week (all models)\n\
███████████████████████████▌ 55% used";

        assert_eq!(
            parse_usage_summary(status),
            Some("31% left, resets 18:10".into())
        );
    }

    #[test]
    fn usage_summary_works_without_a_reset_time() {
        assert_eq!(
            parse_usage_summary("5h limit: 50% left"),
            Some("50% left".into())
        );
    }

    #[test]
    fn status_monitors_are_shared_per_agent_type() {
        let mut monitors = StatusMonitors::default();
        monitors.ensure(AgentKind::Codex, "codex-one", "/workspace/one");
        monitors.ensure(AgentKind::Codex, "codex-two", "/workspace/two");
        monitors.ensure(AgentKind::Claude, "claude-one", "/workspace/three");

        assert_eq!(
            monitors.get(AgentKind::Codex).unwrap().container,
            "codex-one"
        );
        assert_eq!(
            monitors.get(AgentKind::Claude).unwrap().container,
            "claude-one"
        );
    }

    #[test]
    fn usage_footer_adds_a_bar_for_the_percent_left() {
        assert_eq!(
            usage_footer("50% left, resets 19:30", Some(50), 80),
            "50% left [==========----------], resets 19:30"
        );
        assert_eq!(usage_footer("usage pending", None, 80), "usage pending");
    }

    #[test]
    fn clock_times_are_normalized_to_twenty_four_hours() {
        assert_eq!(normalize_clock_time("6:10pm"), Some("18:10".into()));
        assert_eq!(normalize_clock_time("12:05am"), Some("00:05".into()));
        assert_eq!(normalize_clock_time("(09:30)"), Some("09:30".into()));
        assert_eq!(normalize_clock_time("25:00"), None);
    }

    #[test]
    fn codex_status_submit_is_delayed_until_after_command_input() {
        assert_eq!(
            AgentKind::Codex.status_submit_delay(),
            Some(CODEX_STATUS_SUBMIT_DELAY)
        );
        assert_eq!(AgentKind::Claude.status_submit_delay(), None);
    }

    #[test]
    fn codex_uses_outer_scrollback() {
        assert_eq!(AgentKind::Codex.scroll_owner(), ScrollOwner::Outer);
        assert_eq!(AgentKind::Claude.scroll_owner(), ScrollOwner::Child);
        assert_eq!(AgentKind::Other.scroll_owner(), ScrollOwner::Child);
    }

    #[test]
    fn terminal_style_preserves_vt_colors_and_attributes() {
        let mut parser = vt100::Parser::new(1, 1, 0);
        parser.process(b"\x1b[1;3;4;7;38;5;42;48;2;1;2;3mX");
        let style = TerminalStyle::from(parser.screen().cell(0, 0).unwrap());

        assert_eq!(style.foreground, Color::AnsiValue(42));
        assert_eq!(style.background, Color::Rgb { r: 1, g: 2, b: 3 });
        assert!(style.bold);
        assert!(style.italic);
        assert!(style.underline);
        assert!(style.inverse);
    }

    #[test]
    fn ctrl_keys_are_encoded_for_the_pty() {
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(event), Some(vec![3]));
    }

    #[test]
    fn ctrl_c_and_ctrl_w_close_tui_sessions() {
        assert!(is_close_session_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(is_close_session_key(KeyEvent::new(
            KeyCode::Char('w'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_close_session_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn ctrl_j_and_ctrl_k_navigate_tui_sessions() {
        assert_eq!(
            session_direction(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Some(SessionDirection::Next)
        );
        assert_eq!(
            session_direction(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
            Some(SessionDirection::Previous)
        );
        assert_eq!(
            session_direction(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn session_navigation_stops_at_list_boundaries() {
        assert_eq!(session_selection(0, 3, SessionDirection::Previous), 0);
        assert_eq!(session_selection(0, 3, SessionDirection::Next), 1);
        assert_eq!(session_selection(2, 3, SessionDirection::Next), 2);
        assert_eq!(session_selection(0, 0, SessionDirection::Next), 0);
    }

    #[test]
    fn f4_and_ctrl_v_paste_clipboard_images() {
        assert!(is_paste_image_key(KeyEvent::new(
            KeyCode::F(4),
            KeyModifiers::NONE
        )));
        assert!(is_paste_image_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_paste_image_key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn clipboard_images_must_be_png_and_within_the_size_limit() {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(b"contents");
        assert!(validate_png_bytes(&png).is_ok());
        assert!(validate_png_bytes(b"not an image").is_err());

        let oversized = vec![0_u8; MAX_CLIPBOARD_IMAGE_BYTES + 1];
        assert!(validate_png_bytes(&oversized).is_err());
    }

    #[test]
    fn screenshot_uploads_are_ignored_without_duplicate_entries() {
        let temp = tempfile::tempdir().unwrap();
        let agentbox = temp.path().join(".agentbox");
        fs::create_dir_all(&agentbox).unwrap();
        fs::write(agentbox.join(".gitignore"), "env\n").unwrap();

        ensure_uploads_ignored(temp.path()).unwrap();
        ensure_uploads_ignored(temp.path()).unwrap();

        let contents = fs::read_to_string(agentbox.join(".gitignore")).unwrap();
        assert_eq!(contents, "env\nuploads/\n");
    }

    #[test]
    fn mouse_wheel_is_encoded_for_the_nested_terminal() {
        assert_eq!(
            encode_mouse_scroll(
                true,
                4,
                2,
                KeyModifiers::NONE,
                vt100::MouseProtocolEncoding::Sgr
            ),
            Some(b"\x1b[<64;5;3M".to_vec())
        );
        assert_eq!(
            encode_mouse_scroll(
                false,
                0,
                0,
                KeyModifiers::CONTROL,
                vt100::MouseProtocolEncoding::Default
            ),
            Some(vec![0x1b, b'[', b'M', 113, 33, 33])
        );
    }

    #[test]
    fn mouse_wheel_moves_local_terminal_scrollback() {
        let mut parser = vt100::Parser::new(3, 20, 100);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

        assert_eq!(parser.screen().scrollback(), 0);
        assert!(scroll_parser_scrollback(&mut parser, true, 3));
        assert_eq!(parser.screen().scrollback(), 2);
        assert!(scroll_parser_scrollback(&mut parser, false, 1));
        assert_eq!(parser.screen().scrollback(), 1);
        assert!(scroll_parser_scrollback(&mut parser, false, 3));
        assert_eq!(parser.screen().scrollback(), 0);
        assert!(!scroll_parser_scrollback(&mut parser, false, 3));
    }

    #[test]
    fn streamed_output_keeps_scrolled_view_anchored() {
        let mut parser = vt100::Parser::new(3, 20, 100);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert!(scroll_parser_scrollback(&mut parser, true, 2));
        let view = parser.screen().contents();

        parser.process(b"\r\nsix\r\nseven");

        assert_eq!(parser.screen().contents(), view);
        assert_eq!(parser.screen().scrollback(), 4);
    }

    #[test]
    fn live_view_follows_streamed_output() {
        let mut parser = vt100::Parser::new(3, 20, 100);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour");

        assert_eq!(parser.screen().scrollback(), 0);
        assert!(parser.screen().contents().contains("four"));
    }

    #[test]
    fn top_anchored_scroll_regions_feed_the_scrollback() {
        // Codex's inline mode inserts transcript lines by scrolling inside a
        // top-anchored scroll region (ratatui's insert_before technique). The
        // vendored vt100 must preserve the lines that scroll off the top of
        // the screen, or the conversation history is lost.
        let mut parser = vt100::Parser::new(10, 20, 100);
        parser.process(b"one\r\ntwo\r\nthree");
        parser.process(b"\x1b[1;5r\x1b[3S\x1b[r");

        parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(parser.screen().scrollback(), 3);
        assert!(parser.screen().contents().contains("one"));
    }

    #[test]
    fn interior_scroll_regions_do_not_feed_the_scrollback() {
        let mut parser = vt100::Parser::new(10, 20, 100);
        parser.process(b"one\r\ntwo\r\nthree");
        parser.process(b"\x1b[2;5r\x1b[3S\x1b[r");

        parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn selection_normalizes_to_reading_order() {
        let backwards = Selection {
            anchor: (5, 10),
            head: (2, 15),
        };
        assert_eq!(backwards.normalized(), ((2, 15), (5, 10)));

        let same_row = Selection {
            anchor: (3, 8),
            head: (3, 2),
        };
        assert_eq!(same_row.normalized(), ((3, 2), (3, 8)));
        assert!(!same_row.is_empty());
        assert!(
            Selection {
                anchor: (1, 1),
                head: (1, 1)
            }
            .is_empty()
        );
    }

    #[test]
    fn selection_extracts_visible_text() {
        let mut parser = vt100::Parser::new(2, 20, 0);
        parser.process(b"hello world\r\nsecond");

        let single_row = Selection {
            anchor: (0, 0),
            head: (0, 4),
        };
        assert_eq!(
            selection_text(&parser, single_row).as_deref(),
            Some("hello")
        );

        let multi_row = Selection {
            anchor: (1, 5),
            head: (0, 6),
        };
        assert_eq!(
            selection_text(&parser, multi_row).as_deref(),
            Some("world\nsecond")
        );

        let empty = Selection {
            anchor: (0, 3),
            head: (0, 3),
        };
        assert_eq!(selection_text(&parser, empty), None);
    }

    #[test]
    fn base64_encodes_clipboard_payloads() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn copy_uses_the_osc52_clipboard_sequence() {
        assert_eq!(osc52_copy_sequence("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn input_returns_local_terminal_to_live_view() {
        let mut parser = vt100::Parser::new(3, 20, 100);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert!(scroll_parser_scrollback(&mut parser, true, 3));

        assert!(reset_parser_scrollback(&mut parser));
        assert_eq!(parser.screen().scrollback(), 0);
        assert!(!reset_parser_scrollback(&mut parser));
    }

    #[test]
    fn new_codex_sessions_use_inline_mode() {
        let command = session_command(
            Path::new("/usr/bin/agentbox"),
            Path::new("/tmp/example-repo"),
            "tui-123-7",
            "codex",
            AgentKind::Codex,
            false,
        );

        assert_eq!(
            command.get_argv(),
            &["/usr/bin/agentbox", "run", "codex", "--", "--no-alt-screen"]
        );
        assert_eq!(
            command
                .get_env("AGENTBOX_SESSION_ID")
                .and_then(|value| value.to_str()),
            Some("tui-123-7")
        );
    }

    #[test]
    fn restored_sessions_use_agent_resume_commands() {
        let claude = session_command(
            Path::new("/usr/bin/agentbox"),
            Path::new("/tmp/example-repo"),
            "tui-claude",
            "claude",
            AgentKind::Claude,
            true,
        );
        assert_eq!(
            claude.get_argv(),
            &["/usr/bin/agentbox", "run", "claude", "--", "--continue"]
        );

        let codex = session_command(
            Path::new("/usr/bin/agentbox"),
            Path::new("/tmp/example-repo"),
            "tui-codex",
            "codex",
            AgentKind::Codex,
            true,
        );
        assert_eq!(
            codex.get_argv(),
            &[
                "/usr/bin/agentbox",
                "run",
                "codex",
                "--",
                "--no-alt-screen",
                "resume",
                "--last"
            ]
        );

        let custom = session_command(
            Path::new("/usr/bin/agentbox"),
            Path::new("/tmp/example-repo"),
            "tui-custom",
            "my-agent --flag",
            AgentKind::Other,
            true,
        );
        assert_eq!(
            custom.get_argv(),
            &["/usr/bin/agentbox", "run", "my-agent --flag"]
        );
    }

    #[test]
    fn status_processes_run_inside_the_session_container_without_a_prompt() {
        let claude = status_process_command("agentbox-demo-tui-1", "/workspace", AgentKind::Claude);
        assert_eq!(
            claude.get_argv(),
            &[
                "docker",
                "exec",
                "-i",
                "-t",
                "-e",
                "TERM=xterm-256color",
                "-w",
                "/workspace",
                "agentbox-demo-tui-1",
                "/bin/sh",
                "-c",
                r#"PATH="$HOME/.agentbox/npm/bin:$PATH"; export PATH; exec "$@""#,
                "agentbox-status",
                "claude",
                "--dangerously-skip-permissions",
            ]
        );

        let codex = status_process_command("agentbox-demo-tui-1", "/workspace", AgentKind::Codex);
        assert_eq!(
            codex.get_argv(),
            &[
                "docker",
                "exec",
                "-i",
                "-t",
                "-e",
                "TERM=xterm-256color",
                "-w",
                "/workspace",
                "agentbox-demo-tui-1",
                "/bin/sh",
                "-c",
                r#"PATH="$HOME/.agentbox/npm/bin:$PATH"; export PATH; exec "$@""#,
                "agentbox-status",
                "codex",
                "--dangerously-bypass-approvals-and-sandbox",
                "--no-alt-screen",
            ]
        );
    }

    #[test]
    fn status_checks_use_only_local_slash_commands() {
        assert_eq!(AgentKind::Claude.command(), Some("/usage"));
        assert_eq!(AgentKind::Codex.command(), Some("/status"));
        assert_eq!(STATUS_INITIAL_DELAY, Duration::from_secs(5));
        assert_eq!(STATUS_POLL_INTERVAL, Duration::from_secs(30));
        assert_eq!(STATUS_RETRY_DELAY, Duration::from_secs(2));
    }

    #[test]
    fn status_checker_waits_for_each_agent_ui() {
        assert!(status_screen_ready(
            AgentKind::Claude,
            "Welcome to Claude Code\n❯"
        ));
        assert!(status_screen_ready(
            AgentKind::Codex,
            "OpenAI Codex (v1.2.3)\n›"
        ));
        assert!(!status_screen_ready(
            AgentKind::Codex,
            "OpenAI Codex (v1.2.3)"
        ));
    }

    #[test]
    fn lazygit_runs_directly_in_the_selected_host_repo() {
        let repo = Path::new("/tmp/example-repo");
        let command = lazygit_command(repo);

        assert_eq!(command.get_argv()[0], "lazygit");
        assert_eq!(command.get_cwd().map(Path::new), Some(repo));
    }
}
