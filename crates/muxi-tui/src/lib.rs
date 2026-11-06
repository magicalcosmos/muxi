//! Terminal UI, Vim input handling, and lightweight text editing.

pub mod dialog;

use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    cursor::SetCursorStyle,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use muxi_provider::{Provider, ProviderEvent, ProviderRequest, StopReason};
use muxi_store::WorkspaceEntry;
use muxi_tools::{ToolError, Workspace};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

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
    Completed(StopReason),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    /// Emacs-style minibuffer for `C-x f` file paths.
    FilePrompt,
    /// Focus on the workspace sidebar list.
    Sidebar,
    /// Sidebar search box: typing filters the workspace list.
    SidebarSearch,
    /// Emacs-style minibuffer for adding a workspace path.
    WorkspacePrompt,
}

impl Mode {
    const fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Command => "COMMAND",
            Self::FilePrompt => "FILE",
            Self::Sidebar => "SIDEBAR",
            Self::SidebarSearch => "SEARCH",
            Self::WorkspacePrompt => "WORKSPACE",
        }
    }

    /// Contextual hint text shown on the hint line next to the mode.
    const fn hints(self, file_open: bool) -> &'static str {
        match self {
            Self::Normal if file_open => "hjkl move · i edit · :w save · :bd close · t t sidebar",
            Self::Normal => "i compose · : command · t t sidebar · C-x f file · C-w w switch",
            Self::Insert if file_open => "enter newline · esc normal",
            Self::Insert => "enter send · shift+enter newline · esc normal",
            Self::Command => "enter run · esc cancel",
            Self::FilePrompt => "enter open · esc cancel",
            Self::Sidebar => {
                "j/k select · enter activate · a add · d remove · / search · t t close · esc back"
            }
            Self::SidebarSearch => "type to filter · up/down select · enter activate · esc clear",
            Self::WorkspacePrompt => "enter pick folder · esc cancel",
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

/// Number of rows `text` occupies when wrapped to `width` display columns,
/// counting each `\n` segment separately.
fn wrapped_line_count(text: &str, width: u16) -> usize {
    let width = usize::from(width).max(1);
    text.split('\n')
        .map(|segment| {
            let columns = unicode_width::UnicodeWidthStr::width(segment);
            (columns / width + usize::from(columns % width != 0)).max(1)
        })
        .sum()
}

/// Text cut to at most `max` display columns, ending with `…` when truncated.
fn truncate_to_width(text: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if text.width() <= max {
        return text.to_owned();
    }
    let limit = max.saturating_sub(1);
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > limit {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Busy,
}

#[derive(Debug)]
struct App {
    /// Workspace-safe file boundary rooted at the active workspace.
    workspace: Workspace,
    /// Sidebar entries; requests operate inside `workspaces[active].path`.
    workspaces: Vec<WorkspaceEntry>,
    /// Index into `workspaces` of the workspace requests operate in.
    active: usize,
    /// Sidebar cursor position while the sidebar has focus.
    sidebar_selected: usize,
    sidebar_visible: bool,
    /// `SQLite` state database the workspace list is persisted to; `None`
    /// keeps it in memory only (tests).
    store_path: Option<PathBuf>,
    /// Name typed in the workspace prompt, waiting for the directory picker
    /// to run. Drained by the run loop, which suspends the TUI around it.
    pending_pick: Option<String>,
    mode: Mode,
    command: String,
    input: String,
    message: String,
    /// Last sent prompt, shown as the chat bar above the reply.
    last_prompt: Option<String>,
    /// When the running turn started; drives the spinner line.
    turn_started: Option<Instant>,
    /// Sidebar search filter; empty shows every workspace.
    sidebar_query: String,
    response: String,
    phase: Phase,
    last_stop_reason: Option<StopReason>,
    last_error: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    pending_prompt: Option<String>,
    /// Set while a `C-x` prefix waits for its next key.
    ctrl_x_pending: bool,
    /// Set while a `C-w` prefix waits for its next key.
    ctrl_w_pending: bool,
    /// Set while a Normal-mode `t` waits to become `t t` (toggle sidebar).
    t_pending: bool,
    file: Option<FileBuffer>,
    should_quit: bool,
    context: TuiContext,
}

impl App {
    fn new(
        workspace: Workspace,
        workspaces: Vec<WorkspaceEntry>,
        active: usize,
        store_path: Option<PathBuf>,
        context: TuiContext,
    ) -> Self {
        let message = if context.provider_label == "mock" {
            "No provider configured. Create muxi.toml to connect a model. Press i to compose."
                .to_owned()
        } else {
            "Press i to compose, Enter sends, Shift+Enter newline, :q quits.".to_owned()
        };
        Self {
            workspace,
            workspaces,
            active,
            sidebar_selected: active,
            sidebar_visible: false,
            store_path,
            pending_pick: None,
            mode: Mode::Normal,
            command: String::new(),
            input: String::new(),
            message,
            last_prompt: None,
            turn_started: None,
            sidebar_query: String::new(),
            response: String::new(),
            phase: Phase::Idle,
            last_stop_reason: None,
            last_error: None,
            input_tokens: 0,
            output_tokens: 0,
            pending_prompt: None,
            ctrl_x_pending: false,
            ctrl_w_pending: false,
            t_pending: false,
            file: None,
            should_quit: false,
            context,
        }
    }

    /// Root of the workspace all requests and file operations use.
    fn active_root(&self) -> &Path {
        self.workspace.root()
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // Alt+O (Cmd+O on macOS, where the terminal forwards it) toggles the
        // workspace sidebar from any mode.
        if key.code == KeyCode::Char('o')
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::SUPER))
        {
            self.toggle_sidebar();
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

        // Vim-style window prefix: C-w w switches between the two buffers
        // (main window and workspace sidebar).
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('w') {
            self.ctrl_w_pending = true;
            "C-w - (w: switch buffer)".clone_into(&mut self.message);
            return;
        }
        if self.ctrl_w_pending {
            self.ctrl_w_pending = false;
            if key.code == KeyCode::Char('w') && key.modifiers.is_empty() {
                self.switch_buffer();
                return;
            }
            String::new().clone_into(&mut self.message);
        }

        match self.mode {
            Mode::Normal => self.normal_key(key),
            Mode::Insert => self.insert_key(key),
            Mode::Command => self.command_key(key),
            Mode::FilePrompt => self.file_prompt_key(key),
            Mode::Sidebar => self.sidebar_key(key),
            Mode::SidebarSearch => self.sidebar_search_key(key),
            Mode::WorkspacePrompt => self.workspace_prompt_key(key),
        }
    }

    fn sidebar_key(&mut self, key: KeyEvent) {
        if self.t_toggle_key(&key) {
            return;
        }
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => self.sidebar_step(false),
            KeyCode::Char('k') | KeyCode::Up => self.sidebar_step(true),
            KeyCode::Enter => self.activate_workspace(self.sidebar_selected),
            KeyCode::Char('/') => self.mode = Mode::SidebarSearch,
            KeyCode::Char('a') => {
                self.command.clear();
                self.mode = Mode::WorkspacePrompt;
                "Workspace name (Enter picks a folder, Esc cancels):".clone_into(&mut self.message);
            }
            KeyCode::Char('d') => self.remove_workspace(self.sidebar_selected),
            _ => {}
        }
    }

    /// Keys inside the sidebar search box: everything printable is query
    /// text; arrows move within the filtered list.
    fn sidebar_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.sidebar_query.clear();
                self.refocus_sidebar_selection();
                self.mode = Mode::Sidebar;
            }
            KeyCode::Enter => {
                if self.sidebar_matches().is_empty() {
                    "No matching workspace.".clone_into(&mut self.message);
                } else {
                    self.activate_workspace(self.sidebar_selected);
                }
            }
            KeyCode::Up => self.sidebar_step(true),
            KeyCode::Down => self.sidebar_step(false),
            KeyCode::Backspace => {
                self.sidebar_query.pop();
                self.refocus_sidebar_selection();
            }
            KeyCode::Char(character) => {
                self.sidebar_query.push(character);
                self.refocus_sidebar_selection();
            }
            _ => {}
        }
    }

    /// Indices into `workspaces` matching the sidebar search, in list order.
    fn sidebar_matches(&self) -> Vec<usize> {
        let query = self.sidebar_query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.workspaces.len()).collect();
        }
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.name.to_lowercase().contains(&query)
                    || entry.path.to_string_lossy().to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Moves the sidebar cursor one entry up (`true`) or down within the
    /// entries matching the search query, clamped at the ends.
    fn sidebar_step(&mut self, up: bool) {
        let matches = self.sidebar_matches();
        if matches.is_empty() {
            return;
        }
        let position = matches
            .iter()
            .position(|&index| index == self.sidebar_selected)
            .unwrap_or(0);
        let next = if up {
            position.saturating_sub(1)
        } else {
            (position + 1).min(matches.len() - 1)
        };
        self.sidebar_selected = matches[next];
    }

    /// Keeps the sidebar cursor on a matching entry after the query changed;
    /// jumps to the first match when the current entry no longer matches.
    fn refocus_sidebar_selection(&mut self) {
        let matches = self.sidebar_matches();
        if !matches.is_empty() && !matches.contains(&self.sidebar_selected) {
            self.sidebar_selected = matches[0];
        }
    }

    /// Shows or hides the workspace sidebar, focusing it when shown.
    fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        if self.sidebar_visible {
            self.sidebar_query.clear();
            self.sidebar_selected = self.active;
            self.mode = Mode::Sidebar;
            "Workspaces: a add, Enter activate, d remove, / search, Esc back."
                .clone_into(&mut self.message);
        } else {
            self.mode = Mode::Normal;
            String::new().clone_into(&mut self.message);
        }
    }

    /// Vim `C-w w`: cycles focus between the two buffers — the main window
    /// and the workspace sidebar. Opens the sidebar when it is hidden.
    fn switch_buffer(&mut self) {
        if !self.sidebar_visible {
            self.toggle_sidebar();
            return;
        }
        if matches!(self.mode, Mode::Sidebar | Mode::SidebarSearch) {
            self.sidebar_query.clear();
            self.refocus_sidebar_selection();
            self.mode = Mode::Normal;
            "Buffer: main window.".clone_into(&mut self.message);
        } else {
            self.mode = Mode::Sidebar;
            "Buffer: workspace sidebar (C-w w switches back).".clone_into(&mut self.message);
        }
    }

    fn workspace_prompt_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.command.clear();
                self.mode = Mode::Sidebar;
            }
            KeyCode::Enter => {
                let name = self.command.trim().to_owned();
                if name.is_empty() {
                    "Enter a workspace name first.".clone_into(&mut self.message);
                    return;
                }
                self.command.clear();
                // The run loop suspends the TUI, shows the OS directory
                // picker, and hands the result back via complete_pick.
                self.pending_pick = Some(name);
            }
            KeyCode::Backspace => {
                self.command.pop();
            }
            KeyCode::Char(character) => self.command.push(character),
            _ => {}
        }
    }

    /// Finishes the add flow after the directory picker closed. Restores the
    /// sidebar focus either way.
    fn complete_workspace_pick(&mut self, name: &str, picked: Option<PathBuf>) {
        self.mode = Mode::Sidebar;
        let Some(path) = picked else {
            "Workspace selection cancelled.".clone_into(&mut self.message);
            return;
        };
        self.add_workspace(name, &path);
    }

    /// Makes `index` the workspace requests and file operations use. Fails
    /// while a turn is running so every request stays inside one workspace.
    fn activate_workspace(&mut self, index: usize) {
        let Some(index) = self.activation_target(index) else {
            return;
        };
        let root = self.workspaces[index].path.clone();
        match Workspace::new(&root) {
            Ok(workspace) => self.workspace = workspace,
            Err(error) => {
                self.message = format!("Cannot open workspace: {error}");
                return;
            }
        }
        self.active = index;
        self.sidebar_selected = index;
        // Buffers from the previous workspace are outside the new boundary.
        self.file = None;
        self.message = format!("Workspace: {}", root.display());
    }

    /// Shared activation preconditions; `None` means the activation must not
    /// proceed. Same-index calls are silent no-ops.
    fn activation_target(&mut self, index: usize) -> Option<usize> {
        let index = index.min(self.workspaces.len().saturating_sub(1));
        if index == self.active {
            return None;
        }
        if self.phase == Phase::Busy {
            "A turn is running; the workspace stays locked until it finishes."
                .clone_into(&mut self.message);
            return None;
        }
        Some(index)
    }

    /// Adds a named workspace directory (canonicalized, deduplicated by
    /// path) and activates it.
    fn add_workspace(&mut self, name: &str, path: &Path) {
        let Ok(added) = Workspace::new(path) else {
            self.message = format!(
                "Cannot add workspace (no such directory): {}",
                path.display()
            );
            return;
        };
        let root = added.root().to_path_buf();
        if let Some(existing) = self.workspaces.iter().position(|entry| entry.path == root) {
            self.sidebar_selected = existing;
            if self.phase == Phase::Busy {
                self.message = format!("Workspace already listed: {}", root.display());
            } else {
                self.activate_workspace(existing);
            }
            return;
        }
        self.workspaces.push(WorkspaceEntry {
            name: name.to_owned(),
            path: root.clone(),
        });
        self.sidebar_selected = self.workspaces.len() - 1;
        if self.phase == Phase::Busy {
            self.message = format!(
                "Added {} (activates after the running turn finishes)",
                root.display()
            );
        } else {
            self.activate_workspace(self.workspaces.len() - 1);
        }
        self.persist();
    }

    /// Removes a workspace from the sidebar list. The last entry and the
    /// active entry during a turn are protected.
    fn remove_workspace(&mut self, index: usize) {
        if self.workspaces.len() <= 1 {
            "Cannot remove the last workspace.".clone_into(&mut self.message);
            return;
        }
        if index >= self.workspaces.len() {
            return;
        }
        if self.phase == Phase::Busy && index == self.active {
            "A turn is running; the active workspace stays locked until it finishes."
                .clone_into(&mut self.message);
            return;
        }
        let removed = self.workspaces.remove(index);
        if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            self.active = index.min(self.workspaces.len() - 1);
            match Workspace::new(&self.workspaces[self.active].path) {
                Ok(workspace) => self.workspace = workspace,
                Err(error) => self.message = format!("Cannot open workspace: {error}"),
            }
            self.file = None;
        }
        self.sidebar_selected = self.sidebar_selected.min(self.workspaces.len() - 1);
        self.message = format!("Removed workspace: {}", removed.name);
        self.persist();
    }

    /// Writes the workspace list to the `SQLite` state database, best-effort.
    /// The connection is opened per write; sidebar saves are rare.
    fn persist(&mut self) {
        let Some(path) = self.store_path.as_ref() else {
            return;
        };
        let result =
            muxi_store::WorkspaceStore::open(path).and_then(|store| store.save(&self.workspaces));
        if let Err(error) = result {
            self.message = format!("Cannot save workspaces: {error}");
        }
    }

    /// Vim-style `t t` toggles the workspace sidebar from the normal views
    /// and from the focused sidebar itself, so the same double key opens and
    /// closes it. Any other key cancels the pending `t` and is then processed
    /// normally by the caller (not swallowed like a prefix). Returns `true`
    /// when the key was consumed by the toggle.
    fn t_toggle_key(&mut self, key: &KeyEvent) -> bool {
        if self.t_pending {
            self.t_pending = false;
            if key.code == KeyCode::Char('t') && key.modifiers.is_empty() {
                self.toggle_sidebar();
                return true;
            }
            return false;
        }
        if key.code == KeyCode::Char('t') && key.modifiers.is_empty() {
            self.t_pending = true;
            "t - (t: toggle workspace sidebar)".clone_into(&mut self.message);
            return true;
        }
        false
    }

    fn normal_key(&mut self, key: KeyEvent) {
        if self.t_toggle_key(&key) {
            return;
        }
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
        let prompt = self.input.trim().to_owned();
        self.pending_prompt = Some(prompt.clone());
        self.last_prompt = Some(prompt);
        self.input.clear();
        self.response.clear();
        self.last_stop_reason = None;
        self.last_error = None;
        self.phase = Phase::Busy;
        self.turn_started = Some(Instant::now());
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
                match self.resolve_in_workspace(&input) {
                    Ok(path) => match FileBuffer::open(path) {
                        Ok(file) => {
                            self.message = format!("Opened {}", file.path.display());
                            self.file = Some(file);
                        }
                        Err(error) => {
                            self.message = format!("Cannot open file: {error}");
                        }
                    },
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

    /// Resolves a user-entered file path against the active workspace.
    /// Relative paths go through the workspace boundary; absolute paths must
    /// stay inside it, so every request operates on workspace files only.
    fn resolve_in_workspace(&self, input: &str) -> Result<PathBuf, ToolError> {
        let path = Path::new(input);
        if path.is_absolute() {
            let canonical = muxi_tools::canonicalize(path)?;
            if canonical.starts_with(self.active_root()) {
                Ok(canonical)
            } else {
                Err(ToolError::PathEscapesWorkspace(canonical))
            }
        } else {
            self.workspace.resolve(path)
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
                | ProviderEvent::ToolCall(_)
                | ProviderEvent::Finished { .. },
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
            Inbox::Completed(stop_reason) => {
                self.phase = Phase::Idle;
                self.turn_started = None;
                self.last_stop_reason = Some(stop_reason);
                self.last_error = None;
                let outcome = match stop_reason {
                    StopReason::EndTurn | StopReason::StopSequence => None,
                    StopReason::MaxTokens => {
                        Some("Response stopped because the output token limit was reached.")
                    }
                    StopReason::ToolUse => {
                        Some("The model requested a tool, but tool execution is not yet handled.")
                    }
                    StopReason::PauseTurn => {
                        Some("The model paused this turn; continuation is required.")
                    }
                    StopReason::Refusal => Some("The request was refused by the provider."),
                    StopReason::ModelContextWindowExceeded => {
                        Some("Response stopped because the model context window was exhausted.")
                    }
                };
                if let Some(message) = outcome {
                    message.clone_into(&mut self.message);
                } else if self.response.is_empty() {
                    "Turn finished with no output.".clone_into(&mut self.message);
                } else {
                    String::new().clone_into(&mut self.message);
                }
            }
            Inbox::Failed(error) => {
                self.phase = Phase::Idle;
                self.turn_started = None;
                self.last_stop_reason = None;
                self.last_error = Some(error.clone());
                self.message = format!("Turn failed: {error}");
            }
        }
    }

    fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        // Sidebar on the left, main column takes the rest of the width.
        let (main_area, sidebar_area) = if self.sidebar_visible {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(30), Constraint::Min(20)])
                .split(frame.area());
            (columns[1], Some(columns[0]))
        } else {
            (frame.area(), None)
        };

        let header_height =
            u16::try_from(3 + usize::from(!self.message.is_empty())).unwrap_or(u16::MAX);
        let composer_height =
            u16::try_from(2 + self.input.matches('\n').count()).unwrap_or(u16::MAX);
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_height),
                Constraint::Min(5),
                Constraint::Length(1),
                Constraint::Length(composer_height),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(main_area);

        frame.render_widget(self.header_view(), layout[0]);
        self.draw_body(frame, layout[1]);
        frame.render_widget(self.spinner_view(), layout[2]);
        frame.render_widget(self.composer_view(), layout[3]);
        frame.render_widget(self.hint_view(), layout[4]);
        frame.render_widget(self.toolbar_view(layout[5].width), layout[5]);

        let mut search_area = None;
        if let Some(area) = sidebar_area {
            search_area = self.draw_sidebar(frame, area);
        }

        let cursor = match (&self.mode, self.file.as_ref()) {
            (Mode::Insert | Mode::Normal, Some(file)) => Self::file_cursor(file, layout[1]),
            (Mode::SidebarSearch, _) => {
                search_area.map(|area| Self::search_cursor(&self.sidebar_query, area))
            }
            _ => self.composer_cursor(layout[3]),
        };
        if let Some(position) = cursor {
            frame.set_cursor_position(position);
        }
    }

    /// App title block: product line, model/provider line, workspace path,
    /// plus a notice line while `message` is set (styled like a warning bar).
    fn header_view(&self) -> Paragraph<'_> {
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    " ✳ ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Muxi",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" v{}", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(Span::styled(
                format!(" {} · {}", self.context.model, self.context.provider_label),
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                format!(" {}", self.active_root().display()),
                Style::default().fg(Color::Gray),
            )),
        ];
        if !self.message.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(
                    " !! ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.message.as_str(), Style::default().fg(Color::Gray)),
            ]));
        }
        Paragraph::new(lines)
    }

    /// Chat view (last prompt as a full-width bar, reply below) or the open
    /// file buffer.
    fn draw_body(&self, frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect) {
        let Some(file) = self.file.as_ref() else {
            let bar_height = match &self.last_prompt {
                Some(prompt) => {
                    u16::try_from(wrapped_line_count(&format!("> {prompt}"), area.width))
                        .unwrap_or(u16::MAX)
                        .min(area.height)
                }
                None => 0,
            };
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(bar_height), Constraint::Min(0)])
                .split(area);
            if let Some(prompt) = &self.last_prompt {
                let bar = Paragraph::new(format!("> {prompt}"))
                    .style(Style::default().bg(Color::DarkGray).fg(Color::White))
                    .wrap(Wrap { trim: false });
                frame.render_widget(bar, rows[0]);
            }
            if !self.response.is_empty() {
                let lines: Vec<Line> = std::iter::once(Line::from(""))
                    .chain(self.response.lines().map(Line::from))
                    .collect();
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[1]);
            }
            return;
        };
        frame.render_widget(Self::file_view(file), area);
    }

    /// Animated turn indicator: `✳ Spinning… (12s · ↓ 300 tokens)` while a
    /// turn runs, a token summary once idle, blank before the first turn.
    fn spinner_view(&self) -> Paragraph<'_> {
        if self.phase == Phase::Busy {
            let elapsed = self
                .turn_started
                .map_or(Duration::from_millis(0), |start| start.elapsed());
            let frames = ["✳", "✽", "✶", "✻"];
            let frame = frames[((elapsed.as_millis() / 150) % frames.len() as u128) as usize];
            let mut text = format!(
                " {frame} Spinning… ({}s · ↓ {} tokens",
                elapsed.as_secs(),
                self.output_tokens
            );
            if self.response.is_empty() {
                text.push_str(" · still thinking");
            }
            text.push(')');
            return Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }
        if self.output_tokens == 0 {
            return Paragraph::new(Line::from(" "));
        }
        Paragraph::new(Line::from(Span::styled(
            format!(" ↳ {}↑ / {}↓ tokens", self.input_tokens, self.output_tokens),
            Style::default().fg(Color::DarkGray),
        )))
    }

    /// The composer input row: a top rule, an input prefix (`>`, `:`, …) and
    /// the active text.
    fn composer_view(&self) -> Paragraph<'_> {
        let content = match (&self.mode, self.file.as_ref()) {
            (Mode::Command | Mode::FilePrompt | Mode::WorkspacePrompt, _) => vec![
                Self::composer_prefix_span(self.mode),
                Span::raw(self.command.as_str()),
            ],
            (Mode::Insert, None) => vec![
                Self::composer_prefix_span(self.mode),
                Span::raw(self.input.as_str()),
            ],
            (_, Some(file)) => vec![Span::styled(
                format!(" {}", file.path.display()),
                Style::default().fg(Color::DarkGray),
            )],
            _ => vec![Self::composer_prefix_span(self.mode)],
        };
        Paragraph::new(Line::from(content)).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
    }

    /// Input prefix for the active prompt mode, styled like the screenshot's
    /// `>` marker.
    fn composer_prefix_span(mode: Mode) -> Span<'static> {
        let (text, style) = match mode {
            Mode::Command => (":", Style::default().fg(Color::Yellow)),
            Mode::FilePrompt => ("find file: ", Style::default().fg(Color::Yellow)),
            Mode::WorkspacePrompt => ("workspace name: ", Style::default().fg(Color::Yellow)),
            _ => ("> ", Style::default().fg(Color::Gray)),
        };
        Span::styled(text, style)
    }

    /// Hint line under the composer: mode tag plus contextual key hints,
    /// like the screenshot's `❯❯ auto mode on (…) · esc to interrupt` row.
    fn hint_view(&self) -> Paragraph<'_> {
        let hints = self.mode.hints(self.file.is_some());
        let mut spans = vec![
            Span::styled(
                " ❯❯ ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", self.mode.label()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(hints, Style::default().fg(Color::Gray)),
        ];
        if self.phase == Phase::Busy {
            spans.push(Span::styled(
                " · turn running",
                Style::default().fg(Color::DarkGray),
            ));
        }
        Paragraph::new(Line::from(spans))
    }

    /// Bottom pill toolbar: hint pills on the left, workspace and model
    /// pills right-aligned, mirroring the screenshot's button row. Hint
    /// pills are dropped when the column is too narrow to fit them.
    fn toolbar_view(&self, width: u16) -> Paragraph<'_> {
        let pill = |text: &str, fg: Color| {
            Span::styled(
                format!(" {text} "),
                Style::default().bg(Color::DarkGray).fg(fg),
            )
        };
        let root_label = self.active_root().display().to_string();
        // Long paths crowd out the right-aligned pills; keep the tail.
        let root_label = match root_label.char_indices().nth_back(27) {
            Some((index, _)) => format!("…{}", &root_label[index..]),
            None => root_label,
        };
        let right = vec![
            pill(&root_label, Color::Gray),
            pill(&self.context.model, Color::Gray),
        ];
        let right_width: usize = right.iter().map(Span::width).sum();
        let mut spans = vec![pill("✳", Color::Red)];
        let mut used = spans[0].width();
        for candidate in ["i compose", "C-x f file", "alt+o sidebar", ":q quit"] {
            let candidate_width = candidate.chars().count() + 2;
            if used + candidate_width + right_width > usize::from(width) {
                continue;
            }
            spans.push(pill(candidate, Color::White));
            used += candidate_width;
        }
        let gap = usize::from(width).saturating_sub(used + right_width);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
        Paragraph::new(Line::from(spans))
    }

    /// Left sidebar: a search box on top, the `workspace` group label and
    /// the (filtered) workspace list. Returns the search box area so the
    /// run loop can place the terminal cursor inside it.
    fn draw_sidebar(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
    ) -> Option<ratatui::layout::Rect> {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(area);

        let searching = self.mode == Mode::SidebarSearch;
        // Icon only; no placeholder text inside the search box.
        let query_line = Span::styled(
            format!(" ⌕ {}", self.sidebar_query),
            Style::default().fg(Color::White),
        );
        let mut search = Paragraph::new(Line::from(query_line)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        );
        if searching {
            search = search.block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Yellow)),
            );
        }
        frame.render_widget(search, rows[0]);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " workspace",
                Style::default().fg(Color::Gray),
            ))),
            rows[1],
        );

        let mut lines = Vec::new();
        let card_width = usize::from(rows[2].width);
        for index in self.sidebar_matches() {
            let entry = &self.workspaces[index];
            let cursor = index == self.sidebar_selected;
            let active = index == self.active;
            // Non-selected entries render as full-width cards so the path
            // stays readable; the selected entry keeps the plain background.
            let card = if cursor {
                Style::default()
            } else {
                Style::default().bg(Color::DarkGray)
            };
            let mut name = truncate_to_width(&entry.name, card_width.saturating_sub(4));
            name.push_str(&" ".repeat(card_width.saturating_sub(2 + name.width())));
            lines.push(Line::from(vec![
                Span::styled("● ", card.fg(if active { Color::Red } else { Color::Gray })),
                Span::styled(
                    name,
                    card.fg(Color::White).add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                ),
            ]));
            let mut path = format!(
                "  {}",
                truncate_to_width(
                    &entry.path.display().to_string(),
                    card_width.saturating_sub(2),
                )
            );
            path.push_str(&" ".repeat(card_width.saturating_sub(path.width())));
            lines.push(Line::from(Span::styled(path, card.fg(Color::Gray))));
            // Transparent gap between cards.
            lines.push(Line::from(" "));
        }
        frame.render_widget(Paragraph::new(lines), rows[2]);

        searching.then_some(rows[0])
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

    /// Terminal cursor position inside the open file buffer.
    fn file_cursor(file: &FileBuffer, area: ratatui::layout::Rect) -> Option<(u16, u16)> {
        let gutter = u16::try_from(file.gutter_width() + 2).ok()?;
        let x = area.x + gutter + u16::try_from(file.cursor_display_width()).ok()?;
        let y = area.y + 1 + u16::try_from(file.cursor_line).ok()?;
        let x = x.min(area.right().saturating_sub(1));
        let y = y.min(area.bottom().saturating_sub(1));
        Some((x, y))
    }

    /// Terminal cursor position inside the sidebar search box, after the
    /// query text.
    fn search_cursor(query: &str, area: ratatui::layout::Rect) -> (u16, u16) {
        let prefix =
            u16::try_from(unicode_width::UnicodeWidthStr::width(" ⌕ ")).unwrap_or(u16::MAX);
        let width = u16::try_from(unicode_width::UnicodeWidthStr::width(query)).unwrap_or(u16::MAX);
        let x = (area.x + 1 + prefix + width).min(area.right().saturating_sub(1));
        (x, area.y + 1)
    }

    /// Terminal cursor position after the last character of the active
    /// composer text, clamped to the block's inner area. `None` when nothing
    /// is being edited.
    fn composer_cursor(&self, area: ratatui::layout::Rect) -> Option<(u16, u16)> {
        let text = match self.mode {
            Mode::Insert => &self.input,
            Mode::Command | Mode::FilePrompt | Mode::WorkspacePrompt => &self.command,
            Mode::Normal | Mode::Sidebar | Mode::SidebarSearch => return None,
        };
        let prefix = u16::try_from(unicode_width::UnicodeWidthStr::width(
            Self::composer_prefix_span(self.mode).content.as_ref(),
        ))
        .unwrap_or(u16::MAX);
        let line_count = u16::try_from(text.matches('\n').count()).ok()?;
        let last_line = text.rsplit('\n').next().unwrap_or_default();
        let width =
            u16::try_from(unicode_width::UnicodeWidthStr::width(last_line)).unwrap_or(u16::MAX);
        let inner_right = area.right().saturating_sub(1);
        let inner_bottom = area.bottom().saturating_sub(1);
        let x = (area.x + 1 + prefix + width).min(inner_right);
        let y = (area.y + 1 + line_count).min(inner_bottom);
        Some((x, y))
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, TuiError> {
        resume_terminal()?;
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

/// Suspends the TUI so an external process (the OS directory picker) can own
/// the screen: raw mode off, back to the main screen buffer.
fn suspend_terminal() -> Result<(), TuiError> {
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}

/// Reclaims the screen after [`suspend_terminal`].
fn resume_terminal() -> Result<(), TuiError> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, SetCursorStyle::BlinkingBar)?;
    Ok(())
}

/// Runs the TUI against the given provider context.
///
/// `workspace` is the initial active workspace and must already be
/// canonicalized and verified as a directory. `workspaces` is the sidebar
/// list (containing `workspace.root()`), `active` its active index, and
/// `store_path` the optional state file the list is persisted to.
///
/// # Errors
///
/// Returns an error when terminal setup, rendering, or input handling fails.
pub fn run(
    workspace: Workspace,
    workspaces: Vec<WorkspaceEntry>,
    active: usize,
    store_path: Option<PathBuf>,
    context: TuiContext,
) -> Result<(), TuiError> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let mut app = App::new(workspace, workspaces, active, store_path, context);

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
        if let Some(name) = app.pending_pick.take() {
            // Hand the screen to the OS picker, then force a full repaint so
            // no artifacts of the dialog remain in the terminal buffer.
            suspend_terminal()?;
            let picked = dialog::pick_directory();
            resume_terminal()?;
            terminal.clear()?;
            app.complete_workspace_pick(&name, picked);
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
        pump_turn(provider, ProviderRequest { model, prompt }, inbox).await;
    });
}

/// Owns a provider turn from start through its unique terminal outcome.
///
/// Provider events are drained before classifying the result, avoiding the
/// race where the future and its channel close become ready together. A
/// provider that returns success without `Finished` is surfaced as a contract
/// failure rather than leaving the UI busy forever.
async fn pump_turn(
    provider: Arc<dyn Provider>,
    request: ProviderRequest,
    inbox: std::sync::mpsc::Sender<Inbox>,
) {
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
    let turn = provider.stream_turn(request, events_tx, CancellationToken::new());
    tokio::pin!(turn);

    let mut finished = false;
    let result = loop {
        tokio::select! {
            event = events_rx.recv() => {
                match event {
                    Some(event) => {
                        if forward_turn_event(event, &inbox, &mut finished).is_err() {
                            return;
                        }
                    }
                    None => break turn.await,
                }
            }
            result = &mut turn => break result,
        }
    };

    while let Some(event) = events_rx.recv().await {
        if forward_turn_event(event, &inbox, &mut finished).is_err() {
            return;
        }
    }

    match result {
        Ok(()) if !finished => {
            let _ = inbox.send(Inbox::Failed(
                "provider turn ended without a terminal event".to_owned(),
            ));
        }
        Err(error) if !finished => {
            let _ = inbox.send(Inbox::Failed(error.to_string()));
        }
        Ok(()) | Err(_) => {
            // The protocol terminal event owns the UI outcome. A transport
            // error observed afterwards does not turn a completed response
            // into a second terminal state.
        }
    }
}

fn forward_turn_event(
    event: ProviderEvent,
    inbox: &std::sync::mpsc::Sender<Inbox>,
    finished: &mut bool,
) -> Result<(), ()> {
    if let ProviderEvent::Finished { stop_reason } = event {
        if *finished {
            return Ok(());
        }
        *finished = true;
        inbox.send(Inbox::Completed(stop_reason)).map_err(|_| ())
    } else {
        inbox.send(Inbox::Event(event)).map_err(|_| ())
    }
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

    fn test_app(root: &Path) -> App {
        let workspace = Workspace::new(root).expect("workspace");
        let workspaces = vec![WorkspaceEntry {
            name: "default".to_owned(),
            path: workspace.root().to_path_buf(),
        }];
        App::new(workspace, workspaces, 0, None, context())
    }

    fn test_app_with_workspaces(roots: &[&Path], active: usize) -> App {
        let workspaces: Vec<WorkspaceEntry> = roots
            .iter()
            .map(|root| {
                let path = Workspace::new(root)
                    .expect("workspace")
                    .root()
                    .to_path_buf();
                let name = path.file_name().map_or_else(
                    || path.to_string_lossy().into_owned(),
                    |name| name.to_string_lossy().into_owned(),
                );
                WorkspaceEntry { name, path }
            })
            .collect();
        let workspace = Workspace::new(roots[active]).expect("workspace");
        App::new(workspace, workspaces, active, None, context())
    }

    #[test]
    fn normal_mode_enters_insert_mode() {
        let mut app = test_app(Path::new("."));
        app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Insert);
    }

    #[test]
    fn normal_mode_q_does_not_quit() {
        let mut app = test_app(Path::new("."));
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn command_mode_quits_with_q() {
        let mut app = test_app(Path::new("."));
        app.on_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        for character in "q".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.should_quit);
    }

    #[test]
    fn insert_mode_enter_sends_and_shift_enter_newlines() {
        let mut app = test_app(Path::new("."));
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
        assert_eq!(app.last_prompt.as_deref(), Some("hello\nworld"));
        assert!(app.turn_started.is_some(), "spinner clock starts");
        assert_eq!(app.message, "Sending…");
        assert_eq!(app.mode, Mode::Insert);
    }

    #[test]
    fn busy_turn_rejects_new_prompt() {
        let mut app = test_app(Path::new("."));
        app.phase = Phase::Busy;
        app.input = "second".to_owned();
        app.take_prompt();
        assert!(app.pending_prompt.is_none());
        assert_eq!(app.input, "second");
    }

    #[test]
    fn provider_events_update_state() {
        let mut app = test_app(Path::new("."));
        app.phase = Phase::Busy;
        app.on_inbox(Inbox::Event(ProviderEvent::TextDelta("hi ".to_owned())));
        app.on_inbox(Inbox::Event(ProviderEvent::TextDelta("there".to_owned())));
        app.on_inbox(Inbox::Event(ProviderEvent::Usage {
            input_tokens: 10,
            output_tokens: 2,
        }));
        app.on_inbox(Inbox::Completed(StopReason::EndTurn));
        assert_eq!(app.response, "hi there");
        assert_eq!((app.input_tokens, app.output_tokens), (10, 2));
        assert_eq!(app.phase, Phase::Idle);
        assert_eq!(app.last_stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn non_terminal_stop_reasons_explain_the_outcome() {
        for (reason, expected) in [
            (StopReason::MaxTokens, "output token limit"),
            (StopReason::ToolUse, "requested a tool"),
            (StopReason::PauseTurn, "paused this turn"),
            (StopReason::Refusal, "refused"),
            (
                StopReason::ModelContextWindowExceeded,
                "context window was exhausted",
            ),
        ] {
            let mut app = test_app(Path::new("."));
            app.phase = Phase::Busy;
            app.response = "partial".to_owned();
            app.on_inbox(Inbox::Completed(reason));
            assert_eq!(app.phase, Phase::Idle);
            assert!(
                app.message.contains(expected),
                "message was: {}",
                app.message
            );
            assert_eq!(app.response, "partial");
        }
    }

    #[test]
    fn new_prompt_clears_previous_outcome() {
        let mut app = test_app(Path::new("."));
        app.last_stop_reason = Some(StopReason::MaxTokens);
        app.last_error = Some("old".to_owned());
        app.input = "next".to_owned();
        app.take_prompt();
        assert_eq!(app.last_stop_reason, None);
        assert_eq!(app.last_error, None);
    }

    #[test]
    fn failed_turn_resets_phase() {
        let mut app = test_app(Path::new("."));
        app.phase = Phase::Busy;
        app.on_inbox(Inbox::Failed("boom".to_owned()));
        assert_eq!(app.phase, Phase::Idle);
        assert_eq!(app.message, "Turn failed: boom");
        assert_eq!(app.last_error.as_deref(), Some("boom"));
    }

    #[derive(Debug)]
    struct ContractProvider {
        events: Vec<ProviderEvent>,
        fail: Option<String>,
    }

    #[async_trait::async_trait]
    impl Provider for ContractProvider {
        fn capabilities(&self) -> muxi_provider::ProviderCapabilities {
            MockProvider::default().capabilities()
        }

        async fn stream_turn(
            &self,
            _request: ProviderRequest,
            events: tokio::sync::mpsc::Sender<ProviderEvent>,
            _cancel: CancellationToken,
        ) -> Result<(), muxi_provider::ProviderError> {
            for event in self.events.clone() {
                events
                    .send(event)
                    .await
                    .map_err(|_| muxi_provider::ProviderError::Cancelled)?;
            }
            match &self.fail {
                Some(error) => Err(muxi_provider::ProviderError::Failed(error.clone())),
                None => Ok(()),
            }
        }
    }

    async fn run_pump(provider: ContractProvider) -> Vec<Inbox> {
        let (tx, rx) = std::sync::mpsc::channel();
        pump_turn(
            Arc::new(provider),
            ProviderRequest {
                model: "test".to_owned(),
                prompt: "hi".to_owned(),
            },
            tx,
        )
        .await;
        rx.try_iter().collect()
    }

    #[tokio::test]
    async fn successful_provider_without_finished_fails_contract() {
        let inboxes = run_pump(ContractProvider {
            events: vec![ProviderEvent::TextDelta("partial".to_owned())],
            fail: None,
        })
        .await;
        assert!(matches!(
            inboxes[0],
            Inbox::Event(ProviderEvent::TextDelta(_))
        ));
        assert!(
            matches!(&inboxes[1], Inbox::Failed(error) if error.contains("without a terminal"))
        );
    }

    #[tokio::test]
    async fn queued_finished_produces_exactly_one_completion() {
        let inboxes = run_pump(ContractProvider {
            events: vec![
                ProviderEvent::TextDelta("done".to_owned()),
                ProviderEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                },
            ],
            fail: None,
        })
        .await;
        assert_eq!(
            inboxes
                .iter()
                .filter(|inbox| matches!(inbox, Inbox::Completed(_)))
                .count(),
            1
        );
        assert!(
            !inboxes
                .iter()
                .any(|inbox| matches!(inbox, Inbox::Failed(_)))
        );
    }

    #[tokio::test]
    async fn provider_error_after_partial_text_is_forwarded() {
        let inboxes = run_pump(ContractProvider {
            events: vec![ProviderEvent::TextDelta("partial".to_owned())],
            fail: Some("upstream 502".to_owned()),
        })
        .await;
        assert!(matches!(&inboxes[1], Inbox::Failed(error) if error.contains("upstream 502")));
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

    fn ctrl_w_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_w_w_switches_between_buffers() {
        let mut app = test_app(Path::new("."));
        // Hidden sidebar: C-w w opens and focuses it.
        app.on_key(ctrl_w_key());
        assert!(app.ctrl_w_pending, "prefix waits for the next key");
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(app.sidebar_visible);
        assert_eq!(app.mode, Mode::Sidebar);
        // Again: focus returns to the main buffer, sidebar stays open.
        app.on_key(ctrl_w_key());
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(app.sidebar_visible);
        assert_eq!(app.mode, Mode::Normal);
        // And again: back to the sidebar.
        app.on_key(ctrl_w_key());
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Sidebar);
        // A different key after the prefix cancels it; the key itself is
        // still processed by the focused buffer (Esc leaves the sidebar).
        app.on_key(ctrl_w_key());
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!app.ctrl_w_pending);
        assert_eq!(app.mode, Mode::Sidebar, "unmapped key keeps sidebar focus");
    }

    #[test]
    fn t_t_toggles_sidebar_and_other_keys_cancel_pending() {
        let mut app = test_app(Path::new("."));
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(app.t_pending);
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(app.sidebar_visible);
        assert_eq!(app.mode, Mode::Sidebar);
        // `t t` while the sidebar itself is focused closes it again.
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(app.t_pending, "sidebar focus also arms the pending `t`");
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(!app.sidebar_visible);
        assert_eq!(app.mode, Mode::Normal);
        // Toggle back from sidebar-focused normal view.
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(app.sidebar_visible);
        assert_eq!(app.mode, Mode::Sidebar);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Normal);
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(!app.sidebar_visible);
        // `t` followed by another key cancels the pending toggle.
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(!app.sidebar_visible, "cancelled pending must not toggle");
        assert_eq!(app.mode, Mode::Insert);
        assert!(!app.t_pending);
    }

    #[test]
    fn ctrl_x_f_opens_file_prompt_and_file_buffer() {
        let directory = tempfile_dir("ctrl-x");
        let file_path = directory.join("notes.md");
        std::fs::write(&file_path, "alpha\nbeta\ngamma\n").expect("write fixture");

        let mut app = test_app(&directory);
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
        let mut app = test_app(Path::new("."));
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

        let mut app = test_app(&directory);
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

        let mut app = test_app(&directory);
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

        let mut app = test_app(&directory);
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

        let mut app = test_app(&directory);
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
        // Parallel tests share the process id, so a per-call counter keeps
        // concurrent temp directories from colliding.
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "muxi-tui-test-{}-{unique}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("test dir");
        directory
    }

    fn cleanup(paths: &[PathBuf]) {
        for path in paths {
            std::fs::remove_dir_all(path).ok();
        }
    }

    /// Two workspace directories with the first one active.
    fn sidebar_app() -> (App, PathBuf, PathBuf) {
        let first = tempfile_dir("ws-first");
        let second = tempfile_dir("ws-second");
        let app = test_app_with_workspaces(&[&first, &second], 0);
        (app, first, second)
    }

    fn canonical(path: &Path) -> PathBuf {
        Workspace::new(path)
            .expect("workspace")
            .root()
            .to_path_buf()
    }

    #[test]
    fn alt_o_toggles_sidebar_from_any_mode() {
        let mut app = test_app(Path::new("."));
        // From normal mode.
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT));
        assert!(app.sidebar_visible);
        assert_eq!(app.mode, Mode::Sidebar);
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT));
        assert!(!app.sidebar_visible);
        assert_eq!(app.mode, Mode::Normal);
        // From insert mode, without leaving the composer text behind.
        app.mode = Mode::Insert;
        app.input = "draft".to_owned();
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT));
        assert!(app.sidebar_visible);
        assert_eq!(app.mode, Mode::Sidebar);
        assert_eq!(app.input, "draft", "composer text is preserved");
    }

    #[test]
    fn sidebar_navigation_moves_cursor() {
        let (mut app, first, second) = sidebar_app();
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.sidebar_selected, 1);
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.sidebar_selected, 1, "cursor clamps at the last entry");
        app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.sidebar_selected, 0);
        cleanup(&[first, second]);
    }

    #[test]
    fn sidebar_search_filters_and_activates_match() {
        let (mut app, first, second) = sidebar_app();
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::SidebarSearch);
        // In search mode every printable key is query text, so the cursor
        // must jump to the first matching entry ("second" names ws-second).
        type_text(&mut app, "second");
        assert_eq!(app.sidebar_matches(), vec![1]);
        assert_eq!(app.sidebar_selected, 1, "cursor jumps to the first match");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active, 1, "Enter activates the highlighted match");
        cleanup(&[first, second]);
    }

    #[test]
    fn sidebar_search_without_matches_is_a_no_op() {
        let (mut app, first, second) = sidebar_app();
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        type_text(&mut app, "zzz");
        assert!(app.sidebar_matches().is_empty());
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active, 0, "no match means nothing to activate");
        assert!(app.message.contains("No matching workspace"));
        cleanup(&[first, second]);
    }

    #[test]
    fn sidebar_search_esc_clears_query_and_returns() {
        let (mut app, first, second) = sidebar_app();
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        type_text(&mut app, "second");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Sidebar);
        assert!(app.sidebar_query.is_empty(), "Esc clears the filter");
        assert_eq!(app.sidebar_matches().len(), 2, "full list is back");
        cleanup(&[first, second]);
    }

    #[test]
    fn sidebar_search_narrows_from_a_broader_query() {
        let (mut app, first, second) = sidebar_app();
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        // "muxi-tui-test" matches both temp workspaces through their names.
        type_text(&mut app, "muxi-tui-test");
        assert_eq!(app.sidebar_matches().len(), 2);
        for _ in 0..4 {
            app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        assert_eq!(
            app.sidebar_selected, 0,
            "selection stays on a match while the query narrows"
        );
        // Restart the query and narrow to a single entry ("ws-second").
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        type_text(&mut app, "-ws-second");
        assert_eq!(app.sidebar_matches(), vec![1]);
        assert_eq!(app.sidebar_selected, 1);
        cleanup(&[first, second]);
    }

    #[test]
    fn sidebar_enter_activates_workspace_and_closes_file() {
        let (mut app, first, second) = sidebar_app();
        let file = first.join("doc.txt");
        std::fs::write(&file, "content\n").expect("fixture");
        app.file = Some(FileBuffer::open(file).expect("open"));

        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active, 1);
        assert_eq!(
            app.active_root(),
            Workspace::new(&second).expect("ws").root()
        );
        assert!(app.file.is_none(), "buffers from the old workspace close");
        assert_eq!(app.sidebar_selected, 1);

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active, 1, "activating the active entry is a no-op");
        cleanup(&[first, second]);
    }

    #[test]
    fn workspace_switch_locked_while_turn_running() {
        let (mut app, first, second) = sidebar_app();
        app.phase = Phase::Busy;
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active, 0, "workspace stays locked during a turn");
        assert!(app.message.contains("locked"), "message: {}", app.message);
        cleanup(&[first, second]);
    }

    #[test]
    fn workspace_prompt_enters_pick_flow_with_typed_name() {
        let (mut app, first, second) = sidebar_app();
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::WorkspacePrompt);
        type_text(&mut app, "third");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.pending_pick.as_deref(), Some("third"));
        assert_eq!(app.workspaces.len(), 2, "nothing added before the pick");
        // The run loop drains the pending name and hands the picker result
        // back.
        let picked_name = app.pending_pick.take().expect("pending pick");
        let third = canonical(&tempfile_dir("ws-third"));
        app.complete_workspace_pick(&picked_name, Some(third.clone()));
        assert_eq!(app.mode, Mode::Sidebar);
        assert_eq!(app.workspaces.len(), 3);
        assert_eq!(app.workspaces[2].name, "third");
        assert_eq!(app.active, 2);
        assert_eq!(app.sidebar_selected, 2);
        cleanup(&[first, second, third]);
    }

    #[test]
    fn cancelled_pick_returns_to_sidebar_without_changes() {
        let (mut app, first, second) = sidebar_app();
        app.pending_pick = Some("unused".to_owned());
        app.complete_workspace_pick("unused", None);
        assert_eq!(app.mode, Mode::Sidebar);
        assert_eq!(app.workspaces.len(), 2, "cancelled pick adds nothing");
        assert!(
            app.message.contains("cancelled"),
            "message: {}",
            app.message
        );
        cleanup(&[first, second]);
    }

    #[test]
    fn add_duplicate_workspace_activates_existing() {
        let (mut app, first, second) = sidebar_app();
        let canonical_first = canonical(&first);
        app.complete_workspace_pick("again", Some(canonical_first));
        assert_eq!(app.workspaces.len(), 2, "duplicate path must not append");
        assert_eq!(app.active, 0, "duplicate activates the existing entry");
        assert!(
            app.workspaces[0].name.contains("ws-first"),
            "existing entry keeps its name, got: {}",
            app.workspaces[0].name
        );
        cleanup(&[first, second]);
    }

    #[test]
    fn remove_workspace_adjusts_active_and_protects_last() {
        let (mut app, first, second) = sidebar_app();
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.active, 0, "active falls to the remaining workspace");
        app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.workspaces.len(), 1, "the last workspace is protected");
        assert!(app.message.contains("Cannot remove the last workspace"));
        cleanup(&[first, second]);
    }

    #[test]
    fn remove_workspace_locked_while_turn_running() {
        let (mut app, first, second) = sidebar_app();
        app.phase = Phase::Busy;
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.workspaces.len(), 2, "active entry stays during a turn");
        cleanup(&[first, second]);
    }

    #[test]
    fn file_open_is_confined_to_active_workspace() {
        let (mut app, first, second) = sidebar_app();
        let inside = second.join("inner.txt");
        std::fs::write(&inside, "inside\n").expect("fixture");
        let outside = first.join("outer.txt");
        std::fs::write(&outside, "outside\n").expect("fixture");

        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // Relative paths resolve against the active workspace.
        app.on_key(ctrl_x_key(true));
        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        type_text(&mut app, "inner.txt");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let file = app.file.as_ref().expect("inside file must open");
        assert_eq!(
            file.path,
            canonical(&second).join("inner.txt"),
            "relative path must resolve inside the active workspace"
        );

        app.on_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        type_text(&mut app, "bd");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.file.is_none());

        // Absolute paths pointing into another workspace are rejected.
        app.on_key(ctrl_x_key(true));
        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        type_text(&mut app, &outside.to_string_lossy());
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.file.is_none(), "out-of-workspace files must not open");
        assert!(
            app.message.contains("escapes workspace"),
            "message: {}",
            app.message
        );
        cleanup(&[first, second]);
    }

    #[test]
    fn renders_sidebar_with_workspaces() {
        let (mut app, first, second) = sidebar_app();
        app.sidebar_visible = true;
        app.mode = Mode::Sidebar;
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| app.draw(frame)).expect("render");
        assert_eq!(terminal.backend().buffer().area.width, 100);
        cleanup(&[first, second]);
    }

    #[test]
    fn composer_cursor_follows_text_end() {
        let mut app = test_app(Path::new("."));
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
            Some((5, 22))
        );
    }

    #[test]
    fn renders_at_small_size() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let app = test_app(Path::new("."));
        terminal.draw(|frame| app.draw(frame)).expect("render");
        assert_eq!(terminal.backend().buffer().area.width, 80);
    }

    #[test]
    fn truncate_to_width_cuts_long_text() {
        assert_eq!(truncate_to_width("short", 10), "short");
        assert_eq!(truncate_to_width("0123456789", 4), "012…");
        // Two CJK columns (2+2) fit in the 5 columns before the ellipsis.
        assert_eq!(truncate_to_width("中文路径很长很长", 6), "中文…");
        assert_eq!(
            truncate_to_width("ab", 2),
            "ab",
            "exactly at the limit fits"
        );
    }

    #[test]
    fn sidebar_cards_show_truncated_paths() {
        let (mut app, first, second) = sidebar_app();
        // Short display names must stay whole; the long temp paths truncate.
        app.workspaces[0].name = "im".to_owned();
        app.workspaces[1].name = "agent".to_owned();
        app.sidebar_visible = true;
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| app.draw(frame)).expect("render");
        let buffer = terminal.backend().buffer();
        let text: String = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect();
        assert!(text.contains("…"), "long temp paths must truncate: {text}");
        assert!(text.contains("im"), "entry name must stay visible: {text}");
        assert!(
            text.contains("agent"),
            "entry name must stay visible: {text}"
        );
        cleanup(&[first, second]);
    }
}
