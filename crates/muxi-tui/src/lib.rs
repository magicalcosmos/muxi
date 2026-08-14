//! Terminal UI, Vim input handling, and lightweight text editing.

use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    cursor::SetCursorStyle,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use muxi_provider::{Provider, ProviderEvent, ProviderRequest};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// Runtime context handed to the TUI: the provider to send turns to and the
/// labels shown in the status bar.
#[derive(Clone)]
pub struct TuiContext {
    pub provider_label: String,
    pub model: String,
    pub provider: Arc<dyn Provider>,
}

impl std::fmt::Debug for TuiContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiContext")
            .field("provider_label", &self.provider_label)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// Messages delivered from the provider task back to the UI loop.
#[derive(Debug)]
enum Inbox {
    Event(ProviderEvent),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    /// Emacs-style minibuffer for `C-x f` file paths.
    FilePrompt,
}

impl Mode {
    const fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Command => "COMMAND",
            Self::FilePrompt => "FILE",
        }
    }
}

/// An open file being edited in the primary buffer.
#[derive(Debug, Clone)]
struct FileBuffer {
    path: PathBuf,
    lines: Vec<String>,
    cursor_line: usize,
    /// Column as a char index into the current line.
    cursor_col: usize,
}

impl FileBuffer {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(&path)?;
        let lines: Vec<String> = if raw.is_empty() {
            vec![String::new()]
        } else {
            raw.lines().map(str::to_owned).collect()
        };
        Ok(Self {
            path,
            lines,
            cursor_line: 0,
            cursor_col: 0,
        })
    }

    fn save(&self) -> std::io::Result<()> {
        std::fs::write(&self.path, self.lines.join("\n"))
    }

    fn clamp_cursor(&mut self) {
        self.cursor_line = self.cursor_line.min(self.lines.len().saturating_sub(1));
        let line_len = self.lines[self.cursor_line].chars().count();
        self.cursor_col = self.cursor_col.min(line_len);
    }

    /// Display width of the text before the cursor on the current line.
    fn cursor_display_width(&self) -> usize {
        self.lines[self.cursor_line]
            .chars()
            .take(self.cursor_col)
            .map(|character| unicode_width::UnicodeWidthChar::width(character).unwrap_or(0))
            .sum()
    }

    /// Relative line number for display: `0` on the cursor line, the distance
    /// from it everywhere else (Vim `relativenumber`).
    fn relative_number(index: usize, cursor: usize) -> usize {
        index.abs_diff(cursor)
    }

    fn gutter_width(&self) -> usize {
        self.lines.len().saturating_sub(1).max(1).to_string().len()
    }
}

fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(index, _)| index)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Busy,
}

impl Phase {
    const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
        }
    }
}

#[derive(Debug)]
struct App {
    workspace: String,
    mode: Mode,
    command: String,
    input: String,
    message: String,
    response: String,
    phase: Phase,
    input_tokens: u64,
    output_tokens: u64,
    pending_prompt: Option<String>,
    /// Set while a `C-x` prefix waits for its next key.
    ctrl_x_pending: bool,
    file: Option<FileBuffer>,
    should_quit: bool,
    context: TuiContext,
}

impl App {
    fn new(workspace: &Path, context: TuiContext) -> Self {
        let message = if context.provider_label == "mock" {
            "No provider configured. Create muxi.toml to connect a model. Press i to compose."
                .to_owned()
        } else {
            "Press i to compose, Enter sends, Shift+Enter newline, :q quits.".to_owned()
        };
        Self {
            workspace: workspace.display().to_string(),
            mode: Mode::Normal,
            command: String::new(),
            input: String::new(),
            message,
            response: String::new(),
            phase: Phase::Idle,
            input_tokens: 0,
            output_tokens: 0,
            pending_prompt: None,
            ctrl_x_pending: false,
            file: None,
            should_quit: false,
            context,
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // Emacs-style prefix: C-x f opens a file prompt.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x') {
            self.ctrl_x_pending = true;
            "C-x - (f: find file)".clone_into(&mut self.message);
            return;
        }
        if self.ctrl_x_pending {
            self.ctrl_x_pending = false;
            if key.code == KeyCode::Char('f') && key.modifiers.is_empty() {
                self.command.clear();
                self.mode = Mode::FilePrompt;
                "Find file (relative to workspace):".clone_into(&mut self.message);
                return;
            }
            String::new().clone_into(&mut self.message);
        }

        match self.mode {
            Mode::Normal => self.normal_key(key),
            Mode::Insert => self.insert_key(key),
            Mode::Command => self.command_key(key),
            Mode::FilePrompt => self.file_prompt_key(key),
        }
    }

    fn normal_key(&mut self, key: KeyEvent) {
        if let Some(file) = self.file.as_mut() {
            match key.code {
                KeyCode::Char('i') => self.mode = Mode::Insert,
                KeyCode::Char(':') => {
                    self.command.clear();
                    self.mode = Mode::Command;
                }
                KeyCode::Char('j') => {
                    file.cursor_line += 1;
                    file.clamp_cursor();
                }
                KeyCode::Char('k') => {
                    file.cursor_line = file.cursor_line.saturating_sub(1);
                    file.clamp_cursor();
                }
                KeyCode::Char('h') => {
                    file.cursor_col = file.cursor_col.saturating_sub(1);
                }
                KeyCode::Char('l') => {
                    file.cursor_col += 1;
                    file.clamp_cursor();
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('i') => self.mode = Mode::Insert,
            KeyCode::Char(':') => {
                self.command.clear();
                self.mode = Mode::Command;
            }
            KeyCode::Char('j') => "Navigation: down".clone_into(&mut self.message),
            KeyCode::Char('k') => "Navigation: up".clone_into(&mut self.message),
            KeyCode::Char('h') => "Navigation: left".clone_into(&mut self.message),
            KeyCode::Char('l') => "Navigation: right".clone_into(&mut self.message),
            _ => {}
        }
    }

    /// Consumes the composer input and queues the prompt for the run loop to
    /// send. Returns nothing when there is no text or a turn is running.
    fn take_prompt(&mut self) {
        if self.phase == Phase::Busy {
            "A turn is already running.".clone_into(&mut self.message);
            return;
        }
        if self.input.trim().is_empty() {
            return;
        }
        self.pending_prompt = Some(self.input.trim().to_owned());
        self.input.clear();
        self.response.clear();
        self.phase = Phase::Busy;
        "Sending…".clone_into(&mut self.message);
    }

    fn insert_key(&mut self, key: KeyEvent) {
        if let Some(file) = self.file.as_mut() {
            match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    let split_at =
                        char_to_byte_index(&file.lines[file.cursor_line], file.cursor_col);
                    let rest = file.lines[file.cursor_line].split_off(split_at);
                    file.lines.insert(file.cursor_line + 1, rest);
                    file.cursor_line += 1;
                    file.cursor_col = 0;
                }
                KeyCode::Backspace => {
                    if file.cursor_col > 0 {
                        let line = &mut file.lines[file.cursor_line];
                        if let Some((index, _)) = line.char_indices().nth(file.cursor_col - 1) {
                            line.remove(index);
                            file.cursor_col -= 1;
                        }
                    } else if file.cursor_line > 0 {
                        let joined = file.lines.remove(file.cursor_line);
                        file.cursor_line -= 1;
                        file.cursor_col = file.lines[file.cursor_line].chars().count();
                        file.lines[file.cursor_line].push_str(&joined);
                    }
                }
                KeyCode::Char(character) => {
                    let line = &mut file.lines[file.cursor_line];
                    let index = line
                        .char_indices()
                        .nth(file.cursor_col)
                        .map_or(line.len(), |(index, _)| index);
                    line.insert(index, character);
                    file.cursor_col += 1;
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.input.push('\n');
                } else {
                    self.take_prompt();
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character) => self.input.push(character),
            _ => {}
        }
    }

    fn command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let command = self.command.trim();
                match command {
                    "q" | "quit" | "wq" => {
                        if command == "wq" {
                            self.save_file();
                        }
                        self.should_quit = true;
                    }
                    "w" | "write" => self.save_file(),
                    "bd" | "close" => self.close_file(),
                    "help" => {
                        "Commands: :q, :w, :bd, :help, :task".clone_into(&mut self.message);
                    }
                    "task" => {
                        "Task view is ready for the next slice.".clone_into(&mut self.message);
                    }
                    _ => self.message = format!("Unknown command: {command}"),
                }
                self.command.clear();
                if !self.should_quit {
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Backspace => {
                self.command.pop();
            }
            KeyCode::Char(character) => self.command.push(character),
            _ => {}
        }
    }

    fn file_prompt_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let input = self.command.trim().to_owned();
                self.command.clear();
                self.mode = Mode::Normal;
                if input.is_empty() {
                    return;
                }
                let workspace = PathBuf::from(&self.workspace);
                let path = if Path::new(&input).is_absolute() {
                    PathBuf::from(&input)
                } else {
                    workspace.join(&input)
                };
                match FileBuffer::open(path) {
                    Ok(file) => {
                        self.message = format!("Opened {}", file.path.display());
                        self.file = Some(file);
                    }
                    Err(error) => {
                        self.message = format!("Cannot open file: {error}");
                    }
                }
            }
            KeyCode::Backspace => {
                self.command.pop();
            }
            KeyCode::Char(character) => self.command.push(character),
            _ => {}
        }
    }

    fn save_file(&mut self) {
        let Some(file) = self.file.as_ref() else {
            "No file to save.".clone_into(&mut self.message);
            return;
        };
        match file.save() {
            Ok(()) => self.message = format!("Saved {}", file.path.display()),
            Err(error) => self.message = format!("Save failed: {error}"),
        }
    }

    fn close_file(&mut self) {
        match self.file.take() {
            Some(file) => self.message = format!("Closed {}", file.path.display()),
            None => "No file to close.".clone_into(&mut self.message),
        }
    }

    fn on_inbox(&mut self, inbox: Inbox) {
        match inbox {
            Inbox::Event(
                ProviderEvent::Started
                | ProviderEvent::ThinkingDelta(_)
                | ProviderEvent::ToolCall(_),
            ) => {}
            Inbox::Event(ProviderEvent::TextDelta(delta)) => {
                self.response.push_str(&delta);
                if self.message == "Sending…" {
                    String::new().clone_into(&mut self.message);
                }
            }
            Inbox::Event(ProviderEvent::Usage {
                input_tokens,
                output_tokens,
            }) => {
                if input_tokens > 0 {
                    self.input_tokens = input_tokens;
                }
                if output_tokens > 0 {
                    self.output_tokens = output_tokens;
                }
            }
            Inbox::Event(ProviderEvent::Finished { .. }) => {
                self.phase = Phase::Idle;
                if self.response.is_empty() {
                    "Turn finished with no output.".clone_into(&mut self.message);
                } else {
                    String::new().clone_into(&mut self.message);
                }
            }
            Inbox::Failed(error) => {
                self.phase = Phase::Idle;
                self.message = format!("Turn failed: {error}");
            }
        }
    }

    fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        let composer_height =
            u16::try_from(3 + self.input.matches('\n').count()).unwrap_or(u16::MAX);
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(2),
                Constraint::Length(composer_height),
            ])
            .split(frame.area());

        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                " MUXI ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(" Workspace: {}", self.workspace)),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Task workspace"),
        );
        frame.render_widget(header, layout[0]);

        let primary = self
            .file
            .as_ref()
            .map_or_else(|| self.body_view(), Self::file_view);
        frame.render_widget(primary, layout[1]);

        let status = Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", self.mode.label()),
                Style::default().fg(Color::Black).bg(Color::Green),
            ),
            Span::raw(format!(
                "  phase: {}  provider: {}  model: {}  tokens: {}/{}",
                self.phase.label(),
                self.context.provider_label,
                self.context.model,
                self.input_tokens,
                self.output_tokens
            )),
        ]))
        .block(Block::default().borders(Borders::TOP | Borders::BOTTOM));
        frame.render_widget(status, layout[2]);

        let composer = match (&self.mode, self.file.is_some()) {
            (Mode::FilePrompt, _) => format!("Find file: {}", self.command),
            (Mode::Normal, true) => {
                "File  |  i edit  j/k/h/l move  C-x f open  :w save  :bd close".to_owned()
            }
            (Mode::Insert, true) => "Editing file  |  Esc back to normal".to_owned(),
            (Mode::Normal, false) => {
                "Normal mode  |  i insert  : command  C-x f find file  :q quit".to_owned()
            }
            (Mode::Insert, false) => self.input.clone(),
            (Mode::Command, _) => self.command.clone(),
        };
        let title = match self.mode {
            Mode::Command => ":",
            Mode::FilePrompt => "Find file",
            _ => "Composer",
        };
        let composer =
            Paragraph::new(composer).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(composer, layout[3]);

        if let Some(position) = self.active_cursor(layout[1], layout[3]) {
            frame.set_cursor_position(position);
        }
    }

    fn file_view(file: &FileBuffer) -> Paragraph<'_> {
        let gutter = file.gutter_width();
        let lines: Vec<Line> = file
            .lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let number = FileBuffer::relative_number(index, file.cursor_line);
                Line::from(vec![
                    Span::styled(
                        format!("{number:>gutter$} "),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(line.as_str()),
                ])
            })
            .collect();
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", file.path.display())),
        )
    }

    fn body_view(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from("Task-centered Vim-modal coding agent"),
            Line::from(""),
            Line::from("No task is running. The event-driven runtime is ready."),
            Line::from(""),
            Line::from(self.message.as_str()),
        ];
        if !self.response.is_empty() {
            lines.push(Line::from(""));
            lines.extend(self.response.lines().map(Line::from));
        }
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Primary buffer"),
        )
    }

    /// Terminal cursor position: inside the open file buffer when editing a
    /// file, otherwise after the composer text.
    fn active_cursor(
        &self,
        file_area: ratatui::layout::Rect,
        composer_area: ratatui::layout::Rect,
    ) -> Option<(u16, u16)> {
        if let Some(file) = self.file.as_ref()
            && matches!(self.mode, Mode::Insert | Mode::Normal)
        {
            let gutter = u16::try_from(file.gutter_width() + 2).ok()?;
            let x = file_area.x + gutter + u16::try_from(file.cursor_display_width()).ok()?;
            let y = file_area.y + 1 + u16::try_from(file.cursor_line).ok()?;
            let x = x.min(file_area.right().saturating_sub(1));
            let y = y.min(file_area.bottom().saturating_sub(1));
            return Some((x, y));
        }
        self.composer_cursor(composer_area)
    }

    /// Terminal cursor position after the last character of the active
    /// composer text, clamped to the block's inner area. `None` in normal
    /// mode, where nothing is being edited.
    fn composer_cursor(&self, area: ratatui::layout::Rect) -> Option<(u16, u16)> {
        let text = match self.mode {
            Mode::Insert => &self.input,
            Mode::Command | Mode::FilePrompt => &self.command,
            Mode::Normal => return None,
        };
        let line_count = u16::try_from(text.matches('\n').count()).ok()?;
        let last_line = text.rsplit('\n').next().unwrap_or_default();
        let width =
            u16::try_from(unicode_width::UnicodeWidthStr::width(last_line)).unwrap_or(u16::MAX);
        let inner_right = area.right().saturating_sub(1);
        let inner_bottom = area.bottom().saturating_sub(1);
        let x = (area.x + 1 + width).min(inner_right);
        let y = (area.y + 1 + line_count).min(inner_bottom);
        Some((x, y))
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, TuiError> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, SetCursorStyle::BlinkingBar)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            stdout(),
            LeaveAlternateScreen,
            SetCursorStyle::DefaultUserShape
        );
    }
}

///
/// # Errors
///
/// Returns an error when terminal setup, rendering, or input handling fails.
pub fn run(workspace: &Path, context: TuiContext) -> Result<(), TuiError> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let mut app = App::new(workspace, context);

    let (inbox_tx, inbox_rx) = std::sync::mpsc::channel::<Inbox>();

    while !app.should_quit {
        terminal.draw(|frame| app.draw(frame))?;
        while let Ok(inbox) = inbox_rx.try_recv() {
            app.on_inbox(inbox);
        }
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            app.on_key(key);
            if let Some(prompt) = app.pending_prompt.take() {
                spawn_turn(&runtime, &app.context, prompt, inbox_tx.clone());
            }
        }
    }

    Ok(())
}

fn spawn_turn(
    runtime: &tokio::runtime::Runtime,
    context: &TuiContext,
    prompt: String,
    inbox: std::sync::mpsc::Sender<Inbox>,
) {
    let provider = context.provider.clone();
    let model = context.model.clone();
    runtime.spawn(async move {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
        let turn = provider.stream_turn(
            ProviderRequest { model, prompt },
            events_tx,
            CancellationToken::new(),
        );
        tokio::pin!(turn);
        loop {
            tokio::select! {
                event = events_rx.recv() => {
                    let Some(event) = event else { break };
                    if inbox.send(Inbox::Event(event)).is_err() {
                        break;
                    }
                }
                result = &mut turn => {
                    if let Err(error) = result {
                        let _ = inbox.send(Inbox::Failed(error.to_string()));
                    }
                    break;
                }
            }
        }
        while let Ok(event) = events_rx.try_recv() {
            if inbox.send(Inbox::Event(event)).is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxi_provider::MockProvider;
    use ratatui::{Terminal, backend::TestBackend};

    fn context() -> TuiContext {
        TuiContext {
            provider: Arc::new(MockProvider::default()),
            provider_label: "mock".to_owned(),
            model: "mock".to_owned(),
        }
    }

    #[test]
    fn normal_mode_enters_insert_mode() {
        let mut app = App::new(Path::new("."), context());
        app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Insert);
    }

    #[test]
    fn normal_mode_q_does_not_quit() {
        let mut app = App::new(Path::new("."), context());
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn command_mode_quits_with_q() {
        let mut app = App::new(Path::new("."), context());
        app.on_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        for character in "q".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.should_quit);
    }

    #[test]
    fn insert_mode_enter_sends_and_shift_enter_newlines() {
        let mut app = App::new(Path::new("."), context());
        app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        for character in "hello".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        for character in "world".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.input, "hello\nworld");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.input.is_empty());
        assert_eq!(app.phase, Phase::Busy);
        assert_eq!(app.pending_prompt.as_deref(), Some("hello\nworld"));
        assert_eq!(app.message, "Sending…");
        assert_eq!(app.mode, Mode::Insert);
    }

    #[test]
    fn busy_turn_rejects_new_prompt() {
        let mut app = App::new(Path::new("."), context());
        app.phase = Phase::Busy;
        app.input = "second".to_owned();
        app.take_prompt();
        assert!(app.pending_prompt.is_none());
        assert_eq!(app.input, "second");
    }

    #[test]
    fn provider_events_update_state() {
        let mut app = App::new(Path::new("."), context());
        app.phase = Phase::Busy;
        app.on_inbox(Inbox::Event(ProviderEvent::TextDelta("hi ".to_owned())));
        app.on_inbox(Inbox::Event(ProviderEvent::TextDelta("there".to_owned())));
        app.on_inbox(Inbox::Event(ProviderEvent::Usage {
            input_tokens: 10,
            output_tokens: 2,
        }));
        app.on_inbox(Inbox::Event(ProviderEvent::Finished {
            stop_reason: muxi_provider::StopReason::EndTurn,
        }));
        assert_eq!(app.response, "hi there");
        assert_eq!((app.input_tokens, app.output_tokens), (10, 2));
        assert_eq!(app.phase, Phase::Idle);
    }

    #[test]
    fn failed_turn_resets_phase() {
        let mut app = App::new(Path::new("."), context());
        app.phase = Phase::Busy;
        app.on_inbox(Inbox::Failed("boom".to_owned()));
        assert_eq!(app.phase, Phase::Idle);
        assert_eq!(app.message, "Turn failed: boom");
    }

    fn ctrl_x_key(prefix: bool) -> KeyEvent {
        KeyEvent::new(
            KeyCode::Char('x'),
            if prefix {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            },
        )
    }

    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
    }

    #[test]
    fn ctrl_x_f_opens_file_prompt_and_file_buffer() {
        let directory = tempfile_dir("ctrl-x");
        let file_path = directory.join("notes.md");
        std::fs::write(&file_path, "alpha\nbeta\ngamma\n").expect("write fixture");

        let mut app = App::new(&directory, context());
        app.on_key(ctrl_x_key(true));
        assert!(app.ctrl_x_pending);
        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::FilePrompt);
        type_text(&mut app, "notes.md");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);
        let file = app.file.as_ref().expect("file buffer");
        assert_eq!(file.lines, vec!["alpha", "beta", "gamma"]);
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn ctrl_x_prefix_is_cancelled_by_other_key() {
        let mut app = App::new(Path::new("."), context());
        app.on_key(ctrl_x_key(true));
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!app.ctrl_x_pending);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn file_buffer_edits_and_saves() {
        let directory = tempfile_dir("edits");
        let file_path = directory.join("edit.txt");
        std::fs::write(&file_path, "hello world\n").expect("write fixture");

        let mut app = App::new(&directory, context());
        app.file = Some(FileBuffer::open(file_path.clone()).expect("open"));
        app.mode = Mode::Insert;
        type_text(&mut app, "X");
        assert_eq!(app.file.as_ref().expect("file").lines[0], "Xhello world");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        type_text(&mut app, "second");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);

        app.on_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        type_text(&mut app, "w");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let saved = std::fs::read_to_string(&file_path).expect("saved file");
        assert_eq!(saved, "X\nsecondhello world");
        assert!(app.message.contains("Saved"));

        app.on_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        type_text(&mut app, "bd");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.file.is_none());
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn file_navigation_moves_cursor() {
        let directory = tempfile_dir("nav");
        let file_path = directory.join("nav.txt");
        std::fs::write(&file_path, "one\ntwo\nthree\n").expect("write fixture");

        let mut app = App::new(&directory, context());
        app.file = Some(FileBuffer::open(file_path).expect("open"));
        for _ in 0..2 {
            app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }
        for _ in 0..5 {
            app.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        }
        let file = app.file.as_ref().expect("file");
        assert_eq!((file.cursor_line, file.cursor_col), (2, 5));
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn relative_numbers_match_vim_relativenumber() {
        assert_eq!(FileBuffer::relative_number(0, 2), 2);
        assert_eq!(FileBuffer::relative_number(2, 2), 0);
        assert_eq!(FileBuffer::relative_number(5, 2), 3);
    }

    #[test]
    fn file_enter_splits_unicode_at_character_cursor() {
        let directory = tempfile_dir("unicode");
        let file_path = directory.join("unicode.txt");
        std::fs::write(&file_path, "你好世界\n").expect("write fixture");

        let mut app = App::new(&directory, context());
        let mut file = FileBuffer::open(file_path).expect("open");
        file.cursor_col = 2;
        app.file = Some(file);
        app.mode = Mode::Insert;
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.file.as_ref().expect("file").lines, vec!["你好", "世界"]);
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn backspace_joins_lines_in_file() {
        let directory = tempfile_dir("join");
        let file_path = directory.join("join.txt");
        std::fs::write(&file_path, "ab\ncd\n").expect("write fixture");

        let mut app = App::new(&directory, context());
        let mut file = FileBuffer::open(file_path).expect("open");
        file.cursor_line = 1;
        file.cursor_col = 0;
        app.file = Some(file);
        app.mode = Mode::Insert;
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        let file = app.file.as_ref().expect("file");
        assert_eq!(file.lines, vec!["abcd"]);
        assert_eq!((file.cursor_line, file.cursor_col), (0, 2));
        std::fs::remove_dir_all(&directory).ok();
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("muxi-tui-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("test dir");
        directory
    }

    #[test]
    fn composer_cursor_follows_text_end() {
        let mut app = App::new(Path::new("."), context());
        assert_eq!(
            app.composer_cursor(ratatui::layout::Rect::new(0, 20, 80, 3)),
            None
        );

        app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        for character in "hi".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        for character in "你".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            app.composer_cursor(ratatui::layout::Rect::new(0, 20, 80, 3)),
            Some((3, 22))
        );
    }

    #[test]
    fn renders_at_small_size() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let app = App::new(Path::new("."), context());
        terminal.draw(|frame| app.draw(frame)).expect("render");
        assert_eq!(terminal.backend().buffer().area.width, 80);
    }
}
