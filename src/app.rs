use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use ratatui::Terminal;

use crate::cli::Args;
use crate::escape::escape_sed_paste;
use crate::files;
use crate::ui;
use crate::worker::{self, WorkerEvent, WorkerHandle, WorkerRequest};

const DEBOUNCE: Duration = Duration::from_millis(120);
const POLL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Expression,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Editing,
    ConfirmApply,
}

pub struct App {
    pub expression: String,
    pub cursor: usize,
    pub focus: Focus,
    pub mode: Mode,
    pub files: Vec<PathBuf>,
    pub sed_bin: PathBuf,
    /// `Some("")` means `-i` with no backup; `None` also means no backup suffix (same apply).
    pub backup_suffix: Option<String>,
    pub diff_lines: Vec<Line<'static>>,
    pub scroll: usize,
    pub loading: bool,
    pub truncated: bool,
    pub preview_changed: usize,
    pub preview_shown: usize,
    pub preview_scanned: usize,
    pub error: Option<String>,
    pub status_message: Option<String>,
    generation: u64,
    pending_preview_at: Option<Instant>,
    worker: WorkerHandle,
    should_quit: bool,
}

impl App {
    pub fn new(args: Args) -> Result<Self> {
        let sed_bin = resolve_sed_bin(&args.sed_bin)?;
        let worker = worker::spawn_worker(sed_bin.clone());
        let files = files::resolve_files(args.files, !args.no_ignore)?;

        let mut app = Self {
            expression: args.expression.unwrap_or_default(),
            cursor: 0,
            focus: Focus::Expression,
            mode: Mode::Editing,
            files,
            sed_bin,
            backup_suffix: args.in_place,
            diff_lines: Vec::new(),
            scroll: 0,
            loading: false,
            truncated: false,
            preview_changed: 0,
            preview_shown: 0,
            preview_scanned: 0,
            error: None,
            status_message: None,
            generation: 0,
            pending_preview_at: None,
            worker,
            should_quit: false,
        };
        app.cursor = app.expression.len();
        app.schedule_preview();
        Ok(app)
    }

    pub fn run(mut self) -> Result<()> {
        if !io::IsTerminal::is_terminal(&io::stdin()) || !io::IsTerminal::is_terminal(&io::stdout())
        {
            anyhow::bail!("sid requires an interactive terminal");
        }

        let mut stdout = io::stdout();
        enable_raw_mode().context("enable raw mode")?;
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
            .context("enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).context("create terminal")?;

        let result = self.event_loop(&mut terminal);

        disable_raw_mode().ok();
        execute!(
            terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        )
        .ok();
        terminal.show_cursor().ok();
        let _ = self.worker.tx.send(WorkerRequest::Shutdown);

        result
    }

    fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        loop {
            terminal.draw(|f| ui::draw(f, self))?;

            self.drain_worker();
            self.maybe_flush_debounce();

            if self.should_quit {
                break;
            }

            if event::poll(POLL)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
                    Event::Paste(text) => self.handle_paste(text),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::ConfirmApply => self.handle_confirm_key(key),
            Mode::Editing => self.handle_editing_key(key),
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.mode = Mode::Editing;
                self.request_apply();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Editing;
                self.status_message = Some("apply cancelled".into());
            }
            _ => {}
        }
    }

    fn handle_editing_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('s') => {
                    if self.files.is_empty() {
                        self.error = Some("no files to apply".into());
                    } else if self.expression.trim().is_empty() {
                        self.error = Some("empty sed expression".into());
                    } else {
                        self.error = None;
                        self.status_message = None;
                        self.mode = Mode::ConfirmApply;
                    }
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Expression => Focus::Diff,
                    Focus::Diff => Focus::Expression,
                };
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Diff => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Diff => {
                let max = self.diff_lines.len().saturating_sub(1);
                if self.scroll < max {
                    self.scroll += 1;
                }
            }
            KeyCode::PageUp if self.focus == Focus::Diff => {
                self.scroll = self.scroll.saturating_sub(20);
            }
            KeyCode::PageDown if self.focus == Focus::Diff => {
                let max = self.diff_lines.len().saturating_sub(1);
                self.scroll = (self.scroll + 20).min(max);
            }
            _ if self.focus == Focus::Expression => self.handle_expression_key(key),
            // When focused on diff, still allow typing to jump back? No — only expression edits.
            KeyCode::Char('k') | KeyCode::Char('j') => {}
            _ => {}
        }
    }

    fn handle_expression_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.expression.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.on_expression_changed();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.expression, self.cursor);
                    self.expression.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.on_expression_changed();
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.expression.len() {
                    let next = next_char_boundary(&self.expression, self.cursor);
                    self.expression.drain(self.cursor..next);
                    self.on_expression_changed();
                }
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.expression, self.cursor);
                }
            }
            KeyCode::Right => {
                if self.cursor < self.expression.len() {
                    self.cursor = next_char_boundary(&self.expression, self.cursor);
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.expression.len(),
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: String) {
        if self.mode != Mode::Editing {
            return;
        }
        // Pasting always targets the expression field (typical sed workflow).
        self.focus = Focus::Expression;
        let escaped = escape_sed_paste(&text);
        if escaped.is_empty() {
            return;
        }
        self.expression.insert_str(self.cursor, &escaped);
        self.cursor += escaped.len();
        self.on_expression_changed();
    }

    fn on_expression_changed(&mut self) {
        self.error = None;
        self.status_message = None;
        self.schedule_preview();
    }

    fn schedule_preview(&mut self) {
        self.pending_preview_at = Some(Instant::now() + DEBOUNCE);
        self.loading = true;
    }

    fn maybe_flush_debounce(&mut self) {
        let Some(at) = self.pending_preview_at else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        self.pending_preview_at = None;
        self.request_preview();
    }

    fn request_preview(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        let _ = self.worker.tx.send(WorkerRequest::Preview {
            generation,
            expression: self.expression.clone(),
            files: self.files.clone(),
        });
        self.loading = true;
    }

    fn request_apply(&mut self) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.status_message = Some("applying…".into());
        let _ = self.worker.tx.send(WorkerRequest::Apply {
            generation,
            expression: self.expression.clone(),
            files: self.files.clone(),
            backup_suffix: self.backup_suffix.clone(),
        });
    }

    fn drain_worker(&mut self) {
        while let Ok(evt) = self.worker.rx.try_recv() {
            match evt {
                WorkerEvent::PreviewReady {
                    generation,
                    lines,
                    truncated,
                    changed,
                    shown,
                    scanned,
                    done,
                    error,
                } => {
                    if generation != self.generation {
                        continue;
                    }
                    self.loading = !done;
                    if let Some(err) = error {
                        // Keep last good diff; surface error.
                        self.error = Some(err);
                    } else {
                        self.error = None;
                        self.diff_lines = lines;
                        self.truncated = truncated;
                        self.preview_changed = changed;
                        self.preview_shown = shown;
                        self.preview_scanned = scanned;
                        if self.scroll >= self.diff_lines.len() {
                            self.scroll = self.diff_lines.len().saturating_sub(1);
                        }
                    }
                }
                WorkerEvent::ApplyFinished { generation, error } => {
                    if generation != self.generation {
                        continue;
                    }
                    self.loading = false;
                    if let Some(err) = error {
                        self.error = Some(err);
                        self.status_message = None;
                    } else {
                        self.error = None;
                        self.status_message = Some("applied".into());
                        self.request_preview();
                    }
                }
            }
        }
    }
}

fn resolve_sed_bin(path: &PathBuf) -> Result<PathBuf> {
    if path.components().count() > 1 || path.is_absolute() {
        return Ok(path.clone());
    }
    which::which(path).with_context(|| format!("sed binary not found: {}", path.display()))
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx - 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}
