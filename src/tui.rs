use crate::diff::{
    ChangeKind, DiffLine, DiffOptions, DiffStats, DiffUpdate, FileDiff, LineTag,
    compute_diff_async, get_file_diff_lines,
};
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

// ─── Palette ──────────────────────────────────────────────────────────────────
const COLOR_ADDED: Color = Color::Rgb(80, 200, 120);
const COLOR_REMOVED: Color = Color::Rgb(220, 80, 80);
const COLOR_MODIFIED: Color = Color::Rgb(230, 170, 50);
const COLOR_UNCHANGED: Color = Color::Rgb(100, 120, 140);
const COLOR_HEADER: Color = Color::Rgb(130, 180, 230);
const COLOR_BG: Color = Color::Rgb(16, 18, 24);
const COLOR_PANEL: Color = Color::Rgb(22, 26, 34);
const COLOR_BORDER: Color = Color::Rgb(45, 55, 70);
const COLOR_BORDER_FOCUSED: Color = Color::Rgb(90, 140, 220);
const COLOR_TEXT: Color = Color::Rgb(210, 218, 230);
const COLOR_DIM: Color = Color::Rgb(90, 100, 115);
const COLOR_SELECTED_BG: Color = Color::Rgb(35, 50, 75);
const COLOR_WATCH: Color = Color::Rgb(100, 200, 255);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ─── App state ────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Focus {
    FileList,
    DiffView,
}

pub struct App {
    // Paths & opts — needed to trigger rescans
    old_path: String,
    new_path: String,
    opts: DiffOptions,

    // Background channel — replaced on each rescan
    rx: mpsc::Receiver<DiffUpdate>,
    shared_tx: Arc<Mutex<mpsc::Sender<DiffUpdate>>>,

    // Live diff state
    files: Vec<FileDiff>,
    stats: DiffStats,
    loading: bool,
    watching: bool, // true once first scan is done and watcher is active
    tick: u8,

    // Debounce watch events
    last_rescan: Option<Instant>,
    // Selection path to restore after rescan
    preserved_sel: Option<std::path::PathBuf>,

    // UI
    list_state: ListState,
    diff_lines: Vec<DiffLine>,
    diff_scroll: usize,
    focus: Focus,
    show_unchanged: bool,
    search_query: String,
    search_mode: bool,
    filtered_indices: Vec<usize>,
    status_msg: Option<String>,
    status_until: Option<Instant>,
    show_help: bool,
}

impl App {
    fn new(
        rx: mpsc::Receiver<DiffUpdate>,
        old_path: String,
        new_path: String,
        opts: DiffOptions,
        shared_tx: Arc<Mutex<mpsc::Sender<DiffUpdate>>>,
    ) -> Self {
        App {
            old_path,
            new_path,
            opts,
            rx,
            shared_tx,
            files: Vec::new(),
            stats: DiffStats::default(),
            loading: true,
            watching: false,
            tick: 0,
            last_rescan: None,
            preserved_sel: None,
            list_state: ListState::default(),
            diff_lines: Vec::new(),
            diff_scroll: 0,
            focus: Focus::FileList,
            show_unchanged: false,
            search_query: String::new(),
            search_mode: false,
            filtered_indices: Vec::new(),
            status_msg: None,
            status_until: None,
            show_help: false,
        }
    }

    // ── Channel drain ─────────────────────────────────────────────────────────

    fn poll_updates(&mut self) -> bool {
        let mut changed = false;

        if let Some(until) = self.status_until {
            if Instant::now() > until {
                self.status_msg = None;
                self.status_until = None;
                changed = true;
            }
        }

        loop {
            match self.rx.try_recv() {
                Ok(DiffUpdate::File(f)) => {
                    self.add_file(f);
                    changed = true;
                }
                Ok(DiffUpdate::Done) => {
                    self.loading = false;
                    self.watching = true;
                    // Reset debounce so events that queued during the scan don't
                    // immediately fire a second rescan
                    self.last_rescan = Some(Instant::now());
                    self.sort_and_rebuild();
                    changed = true;
                    break;
                }
                Ok(DiffUpdate::Error(e)) => {
                    self.flash(format!("⚠  {}", e), 4);
                    self.loading = false;
                    changed = true;
                    break;
                }
                Ok(DiffUpdate::WatchEvent) => {
                    // Already rescanning — eat stale events. The in-progress scan
                    // will reflect the latest state when it finishes.
                    if self.loading {
                        continue;
                    }
                    // Debounce: ignore if we just rescanned within 1200ms
                    let now = Instant::now();
                    let too_soon = self
                        .last_rescan
                        .map(|t| now.duration_since(t) < Duration::from_millis(1200))
                        .unwrap_or(false);
                    if !too_soon {
                        self.trigger_rescan();
                        changed = true;
                    }
                }
                Err(_) => break,
            }
        }
        changed
    }

    // ── Rescan ────────────────────────────────────────────────────────────────

    /// Create a fresh channel, reset all state, and re-launch the diff thread.
    /// The watcher continues using shared_tx which we swap to the new sender.
    fn trigger_rescan(&mut self) {
        self.last_rescan = Some(Instant::now());

        let (tx_new, rx_new) = mpsc::channel::<DiffUpdate>();
        // Swap the shared sender so the watcher uses the new tx going forward
        *self.shared_tx.lock().unwrap() = tx_new.clone();
        self.rx = rx_new;

        // Save current file path so we can re-select it after the scan
        self.preserved_sel = self
            .list_state
            .selected()
            .and_then(|s| self.filtered_indices.get(s))
            .and_then(|&i| self.files.get(i))
            .map(|f| f.rel_path.clone());

        // Reset data state
        self.files.clear();
        self.stats = DiffStats::default();
        self.loading = true;
        self.watching = false;
        self.filtered_indices.clear();
        self.diff_lines.clear();
        self.diff_scroll = 0;
        self.list_state = ListState::default();

        // Flash expires in 3s — doesn't stay forever
        self.flash("🔄 Files changed — rescanning…".to_string(), 3);

        let old = self.old_path.clone();
        let new = self.new_path.clone();
        let opts = self.opts.clone();
        std::thread::spawn(move || {
            compute_diff_async(PathBuf::from(old), PathBuf::from(new), opts, tx_new);
        });
    }

    // ── Incremental add ───────────────────────────────────────────────────────

    fn add_file(&mut self, file: FileDiff) {
        match file.kind {
            ChangeKind::Added => self.stats.added += 1,
            ChangeKind::Removed => self.stats.removed += 1,
            ChangeKind::Modified => self.stats.modified += 1,
            ChangeKind::Unchanged => self.stats.unchanged += 1,
        }
        let passes = self.file_passes_filter(&file);
        let idx = self.files.len();
        self.files.push(file);

        if passes {
            self.filtered_indices.push(idx);
            if self.list_state.selected().is_none() {
                self.list_state.select(Some(0));
                self.load_diff_at(0);
            }
        }
    }

    // ── Sorting & filter ──────────────────────────────────────────────────────

    fn sort_and_rebuild(&mut self) {
        let sel_path: Option<PathBuf> = self
            .list_state
            .selected()
            .and_then(|s| self.filtered_indices.get(s))
            .and_then(|&i| self.files.get(i))
            .map(|f| f.rel_path.clone());

        self.files.sort_by(|a, b| {
            let ord = |k: &ChangeKind| match k {
                ChangeKind::Modified => 0u8,
                ChangeKind::Added => 1,
                ChangeKind::Removed => 2,
                ChangeKind::Unchanged => 3,
            };
            ord(&a.kind)
                .cmp(&ord(&b.kind))
                .then_with(|| a.rel_path.cmp(&b.rel_path))
        });

        self.rebuild_filter();

        if let Some(ref path) = sel_path {
            if let Some(pos) = self
                .filtered_indices
                .iter()
                .position(|&i| self.files.get(i).map(|f| &f.rel_path) == Some(path))
            {
                self.list_state.select(Some(pos));
                self.load_diff_at(pos);
                return;
            }
        }
        if !self.filtered_indices.is_empty() && self.list_state.selected().is_none() {
            self.list_state.select(Some(0));
            self.load_diff_at(0);
        }
    }

    fn rebuild_filter(&mut self) {
        let q = self.search_query.to_lowercase();
        self.filtered_indices = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                if !self.show_unchanged && f.kind == ChangeKind::Unchanged {
                    return false;
                }
                if !q.is_empty() {
                    return f.rel_path.to_string_lossy().to_lowercase().contains(&q);
                }
                true
            })
            .map(|(i, _)| i)
            .collect();
    }

    fn file_passes_filter(&self, file: &FileDiff) -> bool {
        if !self.show_unchanged && file.kind == ChangeKind::Unchanged {
            return false;
        }
        if !self.search_query.is_empty() {
            return file
                .rel_path
                .to_string_lossy()
                .to_lowercase()
                .contains(&self.search_query.to_lowercase());
        }
        true
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    fn selected_file(&self) -> Option<&FileDiff> {
        let &idx = self.filtered_indices.get(self.list_state.selected()?)?;
        self.files.get(idx)
    }

    fn load_diff_at(&mut self, pos: usize) {
        if let Some(&idx) = self.filtered_indices.get(pos) {
            if let Some(file) = self.files.get(idx) {
                self.diff_lines = get_file_diff_lines(file);
                self.diff_scroll = 0;
            }
        }
    }

    fn navigate(&mut self, delta: i32) {
        let len = self.filtered_indices.len();
        if len == 0 {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, len as i32 - 1) as usize;
        self.list_state.select(Some(next));
        self.load_diff_at(next);
    }

    fn scroll_diff(&mut self, delta: i32) {
        let max = self.diff_lines.len().saturating_sub(1);
        let cur = self.diff_scroll as i32;
        self.diff_scroll = (cur + delta).clamp(0, max as i32) as usize;
    }

    fn toggle_unchanged(&mut self) {
        let cur_idx = self
            .list_state
            .selected()
            .and_then(|s| self.filtered_indices.get(s).copied());

        self.show_unchanged = !self.show_unchanged;
        self.rebuild_filter();

        if let Some(old_idx) = cur_idx {
            if let Some(new_pos) = self.filtered_indices.iter().position(|&i| i == old_idx) {
                self.list_state.select(Some(new_pos));
            } else if !self.filtered_indices.is_empty() {
                self.list_state.select(Some(0));
                self.load_diff_at(0);
            }
        }

        let s = &self.stats;
        self.flash(
            if self.show_unchanged {
                format!(
                    "Showing all {} files  +{} -{} ~{} ={}",
                    self.filtered_indices.len(),
                    s.added,
                    s.removed,
                    s.modified,
                    s.unchanged
                )
            } else {
                format!(
                    "Changed only — {} total  +{} -{} ~{}",
                    s.total_changes(),
                    s.added,
                    s.removed,
                    s.modified
                )
            },
            3,
        );
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn flash(&mut self, msg: String, secs: u64) {
        self.status_msg = Some(msg);
        self.status_until = if secs > 0 {
            Some(Instant::now() + Duration::from_secs(secs))
        } else {
            None
        };
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────────

pub fn run_tui(
    rx: mpsc::Receiver<DiffUpdate>,
    old_path: String,
    new_path: String,
    opts: DiffOptions,
    shared_tx: Arc<Mutex<mpsc::Sender<DiffUpdate>>>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(rx, old_path, new_path, opts, shared_tx);

    // Track whether the last frame had an animated spinner so we know when
    // to keep ticking even without user input or new data.
    let mut needs_draw = true;

    loop {
        let data_changed = app.poll_updates();

        // Advance spinner only while loading — and only if we will actually draw
        if app.loading {
            app.tick = app.tick.wrapping_add(1);
        }

        // Only redraw when:
        //   • New data arrived from the background thread
        //   • Spinner is animating (loading or rescan in progress)
        //   • A key event was handled (set needs_draw = true below)
        //   • First frame
        if needs_draw || data_changed || app.loading {
            terminal.draw(|f| draw(f, &mut app))?;
            needs_draw = false;
        }

        // When idle: block up to 100ms waiting for a key.
        // When loading: poll briefly (20ms) so spinner stays smooth.
        let timeout = if app.loading {
            Duration::from_millis(20)
        } else {
            Duration::from_millis(100)
        };

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                needs_draw = true; // always redraw after a keypress

                if app.search_mode {
                    match key.code {
                        KeyCode::Esc => {
                            app.search_mode = false;
                            app.search_query.clear();
                            app.rebuild_filter();
                            if !app.filtered_indices.is_empty() {
                                app.list_state.select(Some(0));
                                app.load_diff_at(0);
                            }
                        }
                        KeyCode::Enter => {
                            app.search_mode = false;
                        }
                        KeyCode::Backspace => {
                            app.search_query.pop();
                            app.rebuild_filter();
                            if !app.filtered_indices.is_empty() {
                                app.list_state.select(Some(0));
                                app.load_diff_at(0);
                            }
                        }
                        KeyCode::Char(c) => {
                            app.search_query.push(c);
                            app.rebuild_filter();
                            if !app.filtered_indices.is_empty() {
                                app.list_state.select(Some(0));
                                app.load_diff_at(0);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if app.show_help {
                    app.show_help = false;
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,

                    KeyCode::Tab => {
                        app.focus = match app.focus {
                            Focus::FileList => Focus::DiffView,
                            Focus::DiffView => Focus::FileList,
                        };
                        app.status_msg = None;
                    }
                    KeyCode::Char('?') => app.show_help = true,
                    KeyCode::Char('/') => {
                        app.search_mode = true;
                        app.search_query.clear();
                        app.status_msg = None;
                    }
                    KeyCode::Char('u') => app.toggle_unchanged(),
                    KeyCode::Char('r') => {
                        app.flash("Rescanning…".to_string(), 0);
                        app.trigger_rescan();
                    }

                    KeyCode::Up | KeyCode::Char('k') => {
                        app.status_msg = None;
                        if app.focus == Focus::FileList {
                            app.navigate(-1);
                        } else {
                            app.scroll_diff(-1);
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.status_msg = None;
                        if app.focus == Focus::FileList {
                            app.navigate(1);
                        } else {
                            app.scroll_diff(1);
                        }
                    }
                    KeyCode::PageUp => {
                        if app.focus == Focus::FileList {
                            app.navigate(-10);
                        } else {
                            app.scroll_diff(-20);
                        }
                    }
                    KeyCode::PageDown => {
                        if app.focus == Focus::FileList {
                            app.navigate(10);
                        } else {
                            app.scroll_diff(20);
                        }
                    }
                    KeyCode::Home | KeyCode::Char('g') => {
                        if app.focus == Focus::FileList {
                            app.list_state.select(Some(0));
                            app.load_diff_at(0);
                        } else {
                            app.diff_scroll = 0;
                        }
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        if app.focus == Focus::FileList {
                            let last = app.filtered_indices.len().saturating_sub(1);
                            app.list_state.select(Some(last));
                            app.load_diff_at(last);
                        } else {
                            app.diff_scroll = app.diff_lines.len().saturating_sub(1);
                        }
                    }
                    KeyCode::Enter => {
                        app.focus = Focus::DiffView;
                        app.status_msg = None;
                    }
                    KeyCode::Esc => {
                        app.focus = Focus::FileList;
                        app.status_msg = None;
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

// ─── Drawing ──────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.size();
    f.render_widget(Block::default().style(Style::default().bg(COLOR_BG)), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, app, chunks[0]);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(chunks[1]);
    draw_file_list(f, app, body[0]);
    draw_diff_view(f, app, body[1]);
    draw_footer(f, app, chunks[2]);

    if app.show_help {
        draw_help_overlay(f, area);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let s = &app.stats;
    let spinner = SPINNER[(app.tick / 3) as usize % SPINNER.len()];

    let mut spans = vec![
        Span::styled(
            " 🍡 mochidetect ",
            Style::default()
                .fg(Color::Rgb(255, 200, 100))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", Style::default().fg(COLOR_BORDER)),
        Span::styled(
            &app.old_path,
            Style::default().fg(Color::Rgb(180, 140, 220)),
        ),
        Span::styled("  →  ", Style::default().fg(COLOR_DIM)),
        Span::styled(
            &app.new_path,
            Style::default().fg(Color::Rgb(120, 190, 240)),
        ),
        Span::styled("  │  ", Style::default().fg(COLOR_BORDER)),
    ];

    if app.loading {
        spans.push(Span::styled(
            format!(
                "{} scanning… +{} -{} ~{} so far",
                spinner, s.added, s.removed, s.modified
            ),
            Style::default()
                .fg(COLOR_HEADER)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            format!(" +{} ", s.added),
            Style::default()
                .fg(COLOR_ADDED)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" -{} ", s.removed),
            Style::default()
                .fg(COLOR_REMOVED)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" ~{} ", s.modified),
            Style::default()
                .fg(COLOR_MODIFIED)
                .add_modifier(Modifier::BOLD),
        ));

        let (unch_label, unch_style) = if app.show_unchanged {
            (
                format!(" ={} ", s.unchanged),
                Style::default().fg(COLOR_UNCHANGED),
            )
        } else {
            (
                format!(" ={} [u] ", s.unchanged),
                Style::default().fg(COLOR_DIM).add_modifier(Modifier::DIM),
            )
        };
        spans.push(Span::styled(unch_label, unch_style));

        // Watch indicator — a subtle pulsing dot
        if app.watching {
            spans.push(Span::styled(
                "  ● watching",
                Style::default().fg(COLOR_WATCH).add_modifier(Modifier::DIM),
            ));
        }
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(COLOR_BORDER))
                .style(Style::default().bg(COLOR_PANEL)),
        ),
        area,
    );
}

fn draw_file_list(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::FileList;
    let border_color = if focused {
        COLOR_BORDER_FOCUSED
    } else {
        COLOR_BORDER
    };

    let title = if app.search_mode {
        format!(" Files  /{}█ ", app.search_query)
    } else if !app.search_query.is_empty() {
        format!(
            " Files  /{} ({}) ",
            app.search_query,
            app.filtered_indices.len()
        )
    } else if app.loading {
        format!(
            " Files ({}) {} ",
            app.filtered_indices.len(),
            SPINNER[(app.tick / 3) as usize % SPINNER.len()]
        )
    } else {
        format!(
            " Files ({}){}  ",
            app.filtered_indices.len(),
            if !app.show_unchanged {
                " · changed only"
            } else {
                ""
            }
        )
    };

    let items: Vec<ListItem> = app
        .filtered_indices
        .iter()
        .map(|&idx| {
            let file = &app.files[idx];
            let (sym, color) = match file.kind {
                ChangeKind::Added => ("+", COLOR_ADDED),
                ChangeKind::Removed => ("-", COLOR_REMOVED),
                ChangeKind::Modified => ("~", COLOR_MODIFIED),
                ChangeKind::Unchanged => ("=", COLOR_UNCHANGED),
            };
            let tc = if file.kind == ChangeKind::Unchanged {
                COLOR_DIM
            } else {
                COLOR_TEXT
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", sym),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "{}{}",
                        file.rel_path.to_string_lossy(),
                        if file.is_binary { " [bin]" } else { "" }
                    ),
                    Style::default().fg(tc),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(title.as_str())
                .title_style(Style::default().fg(COLOR_TEXT).add_modifier(Modifier::BOLD))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(COLOR_PANEL)),
        )
        .highlight_style(
            Style::default()
                .bg(COLOR_SELECTED_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn draw_diff_view(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::DiffView;
    let border_color = if focused {
        COLOR_BORDER_FOCUSED
    } else {
        COLOR_BORDER
    };

    let file_title = if app.loading && app.filtered_indices.is_empty() {
        " Diff View — scanning… ".to_string()
    } else {
        app.selected_file()
            .map(|fi| {
                format!(
                    " {} {} {} ",
                    fi.kind.symbol(),
                    fi.rel_path.to_string_lossy(),
                    fi.kind.label()
                )
            })
            .unwrap_or_else(|| " Diff View ".to_string())
    };

    let inner_h = area.height.saturating_sub(2) as usize;
    let visible: Vec<Line> = if app.loading && app.filtered_indices.is_empty() {
        vec![Line::from(Span::styled(
            "  Scanning for changes…",
            Style::default()
                .fg(COLOR_HEADER)
                .add_modifier(Modifier::ITALIC),
        ))]
    } else {
        app.diff_lines
            .iter()
            .skip(app.diff_scroll)
            .take(inner_h)
            .map(render_diff_line)
            .collect()
    };

    f.render_widget(
        Paragraph::new(Text::from(visible))
            .block(
                Block::default()
                    .title(file_title.as_str())
                    .title_style(Style::default().fg(COLOR_TEXT).add_modifier(Modifier::BOLD))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border_color))
                    .style(Style::default().bg(COLOR_PANEL)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );

    if app.diff_lines.len() > inner_h {
        let mut sb = ScrollbarState::new(app.diff_lines.len()).position(app.diff_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
            Rect {
                x: area.right() - 1,
                y: area.top() + 1,
                width: 1,
                height: area.height - 2,
            },
            &mut sb,
        );
    }
}

fn render_diff_line(dl: &DiffLine) -> Line<'static> {
    match dl.tag {
        LineTag::Header => Line::from(Span::styled(
            dl.content.clone(),
            Style::default()
                .fg(COLOR_HEADER)
                .add_modifier(Modifier::ITALIC),
        )),
        LineTag::Insert => {
            let no = dl
                .new_lineno
                .map(|n| format!("{:4} ", n))
                .unwrap_or_else(|| "     ".into());
            Line::from(vec![
                Span::styled(no, Style::default().fg(Color::Rgb(60, 100, 60))),
                Span::styled(
                    "+ ",
                    Style::default()
                        .fg(COLOR_ADDED)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    dl.content.clone(),
                    Style::default().fg(COLOR_ADDED).bg(Color::Rgb(15, 40, 20)),
                ),
            ])
        }
        LineTag::Delete => {
            let no = dl
                .old_lineno
                .map(|n| format!("{:4} ", n))
                .unwrap_or_else(|| "     ".into());
            Line::from(vec![
                Span::styled(no, Style::default().fg(Color::Rgb(100, 40, 40))),
                Span::styled(
                    "- ",
                    Style::default()
                        .fg(COLOR_REMOVED)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    dl.content.clone(),
                    Style::default()
                        .fg(COLOR_REMOVED)
                        .bg(Color::Rgb(45, 15, 15)),
                ),
            ])
        }
        LineTag::Equal => {
            let no = dl
                .new_lineno
                .map(|n| format!("{:4} ", n))
                .unwrap_or_else(|| "     ".into());
            Line::from(vec![
                Span::styled(no, Style::default().fg(COLOR_DIM)),
                Span::styled("  ", Style::default()),
                Span::styled(
                    dl.content.clone(),
                    Style::default().fg(Color::Rgb(160, 170, 185)),
                ),
            ])
        }
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let msg = if let Some(ref s) = app.status_msg {
        s.clone()
    } else if app.loading {
        format!(
            " {} scanning… ({} found so far)  q Quit",
            SPINNER[(app.tick / 3) as usize % SPINNER.len()],
            app.filtered_indices.len()
        )
    } else if app.focus == Focus::FileList {
        " ↑↓/jk Navigate  Enter/Tab→Diff  / Search  u Unchanged  r Rescan  ? Help  q Quit"
            .to_string()
    } else {
        " ↑↓/jk Scroll  PgUp/PgDn Fast  Esc/Tab→Files  r Rescan  ? Help  q Quit".to_string()
    };

    f.render_widget(
        Paragraph::new(msg.as_str())
            .style(Style::default().fg(COLOR_DIM).bg(Color::Rgb(12, 14, 20))),
        area,
    );
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let width = 58u16;
    let height = 26u16;
    let overlay = Rect::new(
        (area.width.saturating_sub(width)) / 2,
        (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    let rows: Vec<(&str, &str)> = vec![
        ("", ""),
        ("  Navigation", ""),
        ("  ↑ / k", "Move up"),
        ("  ↓ / j", "Move down"),
        ("  PgUp / PgDn", "Fast scroll"),
        ("  g / Home", "Go to top"),
        ("  G / End", "Go to bottom"),
        ("  Tab / Enter", "Switch panel"),
        ("  Esc", "Back to file list"),
        ("", ""),
        ("  Actions", ""),
        ("  /", "Search files by name"),
        ("  u", "Toggle unchanged files"),
        ("  r", "Force rescan now"),
        ("  ?", "This help"),
        ("  q / Ctrl+C", "Quit"),
        ("", ""),
        ("  CLI Flags", ""),
        ("  --gitignore", "Respect .gitignore"),
        ("  -I '*.log|*.lock'", "Ignore glob patterns"),
        ("  -W / --ignore-whitespace", "Ignore whitespace diffs"),
        ("  --no-watch", "Disable live watching"),
        ("  --plain", "Non-TUI output"),
        ("", ""),
        ("  Press any key to close", ""),
    ];

    let text: Vec<Line> = rows
        .iter()
        .map(|(l, r)| {
            if r.is_empty() && !l.is_empty() && !l.starts_with("  Press") {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default()
                        .fg(COLOR_HEADER)
                        .add_modifier(Modifier::UNDERLINED),
                ))
            } else if r.is_empty() {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default()
                        .fg(COLOR_DIM)
                        .add_modifier(Modifier::ITALIC),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("{:<26}", l), Style::default().fg(COLOR_MODIFIED)),
                    Span::styled(r.to_string(), Style::default().fg(COLOR_TEXT)),
                ])
            }
        })
        .collect();

    f.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(18, 22, 32))),
        overlay,
    );
    f.render_widget(
        Paragraph::new(Text::from(text)).block(
            Block::default()
                .title(" 🍡 Help ")
                .title_style(
                    Style::default()
                        .fg(Color::Rgb(255, 200, 100))
                        .add_modifier(Modifier::BOLD),
                )
                .borders(Borders::ALL)
                .border_type(BorderType::Double)
                .border_style(Style::default().fg(Color::Rgb(255, 200, 100)))
                .style(Style::default().bg(Color::Rgb(18, 22, 32))),
        ),
        overlay,
    );
}
