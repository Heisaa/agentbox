use std::{
    io::{self, Read, Stdout, Write},
    path::{Path, PathBuf},
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
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

use crate::{config::Config, project};

const SIDEBAR_WIDTH: u16 = 28;
const SCROLLBACK_ROWS_PER_TICK: usize = 3;
const FRAME_INTERVAL: Duration = Duration::from_millis(50);
const CODEX_STATUS_SUBMIT_DELAY: Duration = Duration::from_millis(100);
const STATUS_CAPTURE_DELAY: Duration = Duration::from_millis(1200);

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
    name: String,
    repo: PathBuf,
    agent: AgentKind,
    terminal: PtyProcess,
    selection: Option<Selection>,
    status_summary: String,
    status_detail: String,
    status_submit_pending: Option<Instant>,
    status_pending: Option<Instant>,
}

struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    dirty: Arc<AtomicBool>,
    exited: Option<u32>,
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

impl Session {
    fn spawn(repo: &Path, rows: u16, cols: u16, sequence: usize) -> Result<Self> {
        let repo = project::find_repo_root_from(repo)?;
        let loaded = Config::load(&repo)?;
        let agent = classify_agent(&loaded.config.agent.command, &loaded.config.agent.default);
        let executable = std::env::current_exe().context("failed to locate agentbox executable")?;
        let command = session_command(&executable, &repo, sequence, agent);
        let terminal = PtyProcess::spawn(command, rows, cols)
            .with_context(|| format!("failed to start agentbox in {}", repo.display()))?;
        let name = repo
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository")
            .to_owned();

        Ok(Self {
            name,
            repo,
            agent,
            terminal,
            selection: None,
            status_summary: "usage not checked".into(),
            status_detail: String::new(),
            status_submit_pending: None,
            status_pending: None,
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
    }

    fn refresh_status(&mut self) {
        if self.status_submit_pending.is_some() {
            return;
        }
        let Some(command) = self.agent.command() else {
            self.status_summary = "usage unavailable for custom agent".into();
            return;
        };
        self.status_pending = None;
        if let Some(delay) = self.agent.status_submit_delay() {
            // Codex must process the slash command before receiving Enter.
            self.write_input(command.as_bytes());
            self.status_submit_pending = Some(Instant::now() + delay);
        } else {
            self.write_input(format!("{command}\r").as_bytes());
            self.status_submit_pending = None;
            self.status_pending = Some(Instant::now());
        }
        self.status_summary = format!("checking {command}...");
    }

    fn poll(&mut self) -> bool {
        let mut changed = self.terminal.poll();
        if self
            .status_submit_pending
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.write_input(b"\r");
            self.status_submit_pending = None;
            self.status_pending = Some(Instant::now());
            changed = true;
        }
        if self
            .status_pending
            .is_some_and(|started| started.elapsed() >= STATUS_CAPTURE_DELAY)
        {
            if let Ok(parser) = self.terminal.parser.lock() {
                let contents = parser.screen().contents();
                self.status_summary = summarize_status(&contents);
                self.status_detail = status_detail(&contents);
            }
            self.status_pending = None;
            changed = true;
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
    NewSession { input: String, error: String },
    Status,
    Help,
    Message { title: String, body: String },
}

struct App {
    sessions: Vec<Session>,
    selected: usize,
    overlay: Overlay,
    sequence: usize,
    width: u16,
    height: u16,
    redraw: bool,
}

impl App {
    fn new(width: u16, height: u16) -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            overlay: Overlay::None,
            sequence: 0,
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

    fn add_session(&mut self, path: &Path) -> Result<()> {
        let (rows, cols) = self.terminal_size();
        self.sequence += 1;
        let session = Session::spawn(path, rows, cols, self.sequence)?;
        self.sessions.push(session);
        self.selected = self.sessions.len() - 1;
        self.redraw = true;
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
}

pub fn run() -> Result<u8> {
    let mut stdout = io::stdout();
    let (width, height) = terminal::size()?;
    let mut app = App::new(width, height);
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    app.add_session(&cwd)?;
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
                    if let Overlay::NewSession { input, .. } = &mut app.overlay {
                        input.push_str(&text);
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

    for session in &mut app.sessions {
        session.terminal.terminate()?;
    }
    if let Overlay::Lazygit(lazygit) = &mut app.overlay {
        lazygit.terminate()?;
    }
    Ok(0)
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    let overlay = std::mem::replace(&mut app.overlay, Overlay::None);
    match overlay {
        Overlay::Lazygit(mut lazygit) => {
            if let Some(bytes) = encode_key(key) {
                lazygit.write(&bytes);
            }
            app.overlay = Overlay::Lazygit(lazygit);
            return Ok(false);
        }
        Overlay::NewSession {
            mut input,
            mut error,
        } => {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let path = expand_path(&input);
                    match app.add_session(&path) {
                        Ok(()) => {}
                        Err(failure) => {
                            error = format!("{failure:#}");
                            app.overlay = Overlay::NewSession { input, error };
                            app.request_redraw();
                        }
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                    app.overlay = Overlay::NewSession { input, error };
                    app.request_redraw();
                }
                KeyCode::Char(character)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    input.push(character);
                    app.overlay = Overlay::NewSession { input, error };
                    app.request_redraw();
                }
                _ => app.overlay = Overlay::NewSession { input, error },
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
            return Ok(true);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('n')) {
            app.overlay = Overlay::NewSession {
                input: String::new(),
                error: String::new(),
            };
            app.request_redraw();
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
                if let Some(session) = app.active_mut() {
                    session.refresh_status();
                }
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

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    let terminal_left = sidebar_width(app.width) + 1;
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
            Overlay::NewSession { input, error } => draw_new_session(stdout, app, input, error)?,
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
    for (index, session) in app.sessions.iter().enumerate() {
        let y = index as u16 + 2;
        if y >= app.height.saturating_sub(2) {
            break;
        }
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
        let state = session
            .terminal
            .exited
            .map(|code| format!("exited {code}"))
            .unwrap_or_else(|| {
                format!(
                    "{} · {}",
                    session.agent.label(),
                    session.repo.parent().unwrap_or(&session.repo).display()
                )
            });
        queue!(
            stdout,
            MoveTo(3, y + 1),
            SetForegroundColor(Color::DarkGrey),
            Print(truncate(&state, width.saturating_sub(5) as usize))
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
            session
                .map(|session| session.status_summary.clone())
                .unwrap_or_else(|| "no session".into()),
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
            "Drag copy  Wheel/PgUp scroll  F1 help  F2 details  F3 lazygit  F5 usage  F6 next  Ctrl-N new  Ctrl-Q quit",
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

fn draw_new_session(stdout: &mut Stdout, app: &App, input: &str, error: &str) -> Result<()> {
    let width = app.width.saturating_sub(8).clamp(20, 76);
    let x = (app.width.saturating_sub(width)) / 2;
    let y = app.height.saturating_sub(7) / 2;
    draw_box(stdout, x, y, width, 7, "New session")?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 2),
        SetForegroundColor(Color::Grey),
        Print("Repository path:"),
        MoveTo(x + 2, y + 3),
        SetForegroundColor(Color::White),
        Print(truncate(input, width.saturating_sub(4) as usize)),
        MoveTo(x + 2, y + 5),
        SetForegroundColor(if error.is_empty() {
            Color::DarkGrey
        } else {
            Color::Red
        }),
        Print(truncate(
            if error.is_empty() {
                "Enter to open, Esc to cancel"
            } else {
                error
            },
            width.saturating_sub(4) as usize
        )),
        MoveTo(
            x + 2 + input.chars().count().min(width.saturating_sub(4) as usize) as u16,
            y + 3
        ),
        Show
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
        .sessions
        .get(app.selected)
        .map(|session| session.status_detail.as_str())
        .filter(|detail| !detail.is_empty())
        .unwrap_or("No captured status yet. Press F5, then open this view again.");
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
    let height = 17.min(app.height.saturating_sub(2));
    let x = (app.width.saturating_sub(width)) / 2;
    let y = (app.height.saturating_sub(height)) / 2;
    draw_box(stdout, x, y, width, height, "Keys")?;
    let lines = [
        "Wheel   scroll the active conversation",
        "PgUp/Dn page through the conversation history",
        "End     jump back to the live view",
        "Drag    select text and copy it to the clipboard",
        "Ctrl-N  open a session in another repository",
        "F6      switch to the next session",
        "F3      open host lazygit in the active repository",
        "F5      run /usage (Claude) or /status (Codex)",
        "F2      show the last captured usage/status view",
        "Ctrl-Q  exit the TUI",
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
    sequence: usize,
    agent: AgentKind,
) -> CommandBuilder {
    let mut command = CommandBuilder::new(executable);
    command.arg("run");
    if agent == AgentKind::Codex {
        command.arg("--");
        command.arg("--no-alt-screen");
    }
    command.cwd(repo);
    command.env("TERM", "xterm-256color");
    command.env(
        "AGENTBOX_SESSION_ID",
        format!("tui-{}-{sequence}", process::id()),
    );
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

fn summarize_status(contents: &str) -> String {
    let relevant = contents
        .lines()
        .map(str::trim)
        .rfind(|line| {
            let lowercase = line.to_ascii_lowercase();
            !line.is_empty()
                && ["usage", "context", "token", "limit", "model", "remaining"]
                    .iter()
                    .any(|keyword| lowercase.contains(keyword))
        })
        .unwrap_or("status captured");
    truncate(relevant, 120)
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
    fn status_summary_prefers_relevant_lines() {
        let status = "header\nModel: codex\nContext remaining: 72%\nprompt";
        assert_eq!(summarize_status(status), "Context remaining: 72%");
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
    fn codex_tui_sessions_disable_the_alternate_screen() {
        let command = session_command(
            Path::new("/usr/bin/agentbox"),
            Path::new("/tmp/example-repo"),
            7,
            AgentKind::Codex,
        );

        assert_eq!(
            command.get_argv(),
            &["/usr/bin/agentbox", "run", "--", "--no-alt-screen"]
        );
    }

    #[test]
    fn lazygit_runs_directly_in_the_selected_host_repo() {
        let repo = Path::new("/tmp/example-repo");
        let command = lazygit_command(repo);

        assert_eq!(command.get_argv()[0], "lazygit");
        assert_eq!(command.get_cwd().map(Path::new), Some(repo));
    }
}
