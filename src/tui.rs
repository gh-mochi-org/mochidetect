use crate::diff::{
    ChangeKind, DiffLine, DiffStats, DiffUpdate, FileDiff, LineTag, get_file_diff_lines,
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
use std::sync::mpsc::Receiver;

// ─── Palette ─────────────────────────────────────────────────────────────────
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

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ─── App state ────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Focus {
    FileList,
    DiffView,
}

pub struct App {
    // Diff data — grows as results stream in
    files: Vec<FileDiff>,
    stats: DiffStats,
    old_path: String,
    new_path: String,

    // Background thread channel
    rx: Receiver<DiffUpdate>,
    loading: bool,
    tick: u8,

    // UI state
    list_state: ListState,
    diff_lines: Vec<DiffLine>,
    diff_scroll: usize,
    focus: Focus,
    show_unchanged: bool,
    search_query: String,
    search_mode: bool,
    /// Indices into `files` that pass the current filter
    filtered_indices: Vec<usize>,
    status_msg: Option<String>,
    show_help: bool,
}

impl App {
    fn new(rx: Receiver<DiffUpdate>, old_path: String, new_path: String) -> Self {
        App {
            files: Vec::new(),
            stats: DiffStats::default(),
            old_path,
            new_path,
            rx,
            loading: true,
            tick: 0,
            list_state: ListState::default(),
            diff_lines: Vec::new(),
            diff_scroll: 0,
            focus: Focus::FileList,
            show_unchanged: false,
            search_query: String::new(),
            search_mode: false,
            filtered_indices: Vec::new(),
            status_msg: None,
            show_help: false,
        }
    }

    // ── Channel drain ─────────────────────────────────────────────────────────

    /// Pull all pending messages from the background thread.
    /// Returns true if anything changed (so caller knows to redraw).
    fn poll_updates(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(DiffUpdate::File(file)) => {
                    self.add_file(file);
                    changed = true;
                }
                Ok(DiffUpdate::Done) => {
                    self.loading = false;
                    self.sort_and_rebuild();
                    changed = true;
                    break;
                }
                Ok(DiffUpdate::Error(e)) => {
                    self.status_msg = Some(format!("⚠  {}", e));
                    self.loading = false;
                    changed = true;
                    break;
                }
                Err(_) => break, // Empty or disconnected
            }
        }
        changed
    }

    /// Add a single incoming FileDiff, update stats + filter incrementally.
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
            // Auto-select and load diff for the very first visible file
            if self.list_state.selected().is_none() {
                self.list_state.select(Some(0));
                self.load_diff_at(0);
            }
        }
    }

    // ── Sorting & filtering ───────────────────────────────────────────────────

    /// Called once when Done arrives — sorts the list and rebuilds indices.
    fn sort_and_rebuild(&mut self) {
        // Remember what was selected so we can re-find it after sort
        let sel_path: Option<PathBuf> = self
            .list_state
            .selected()
            .and_then(|s| self.filtered_indices.get(s))
            .and_then(|&i| self.files.get(i))
            .map(|f| f.rel_path.clone());

        self.files.sort_by(|a, b| {
            let order = |k: &ChangeKind| match k {
                ChangeKind::Modified => 0u8,
                ChangeKind::Added => 1,
                ChangeKind::Removed => 2,
                ChangeKind::Unchanged => 3,
            };
            order(&a.kind)
                .cmp(&order(&b.kind))
                .then_with(|| a.rel_path.cmp(&b.rel_path))
        });

        self.rebuild_filter();

        // Restore selection to same file (now at new position)
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

        // Fallback: select first
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
                    return f
                        .rel_path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&q);
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
        let sel = self.list_state.selected()?;
        let &idx = self.filtered_indices.get(sel)?;
        self.files.get(idx)
    }

    /// Load diff lines for the file at position `pos` in `filtered_indices`.
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
        self.status_msg = Some(if self.show_unchanged {
            format!(
                "Showing all {} files  (+{} added  -{} removed  ~{} modified  ={} unchanged)",
                self.filtered_indices.len(),
                s.added,
                s.removed,
                s.modified,
                s.unchanged
            )
        } else {
            format!(
                "Changed files only — {} total  (+{} added  -{} removed  ~{} modified)",
                s.total_changes(),
                s.added,
                s.removed,
                s.modified
            )
        });
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────────

pub fn run_tui(rx: Receiver<DiffUpdate>, old_path: String, new_path: String) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // App starts immediately in loading state — TUI is up before any I/O
    let mut app = App::new(rx, old_path, new_path);

    loop {
        // Drain background results before drawing (non-blocking)
        app.poll_updates();

        // Advance spinner while loading
        if app.loading {
            app.tick = app.tick.wrapping_add(1);
        }

        terminal.draw(|f| draw(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // ── Search mode ────────────────────────────────────────────
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

                // ── Help overlay ───────────────────────────────────────────
                if app.show_help {
                    app.show_help = false;
                    continue;
                }

                // ── Normal mode ────────────────────────────────────────────
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

    // Left side: title + paths (or spinner while loading)
    let left_spans = if app.loading {
        let spinner = SPINNER[(app.tick / 3) as usize % SPINNER.len()];
        vec![
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
            Span::styled(
                format!("{} scanning… ", spinner),
                Style::default()
                    .fg(Color::Rgb(130, 180, 230))
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    } else {
        let unch_style = if app.show_unchanged {
            Style::default().fg(COLOR_UNCHANGED)
        } else {
            Style::default().fg(COLOR_DIM)
        };
        let unchanged_badge = if app.show_unchanged {
            Span::styled(format!(" ={} ", s.unchanged), unch_style)
        } else {
            Span::styled(
                format!(" ={} [u to show] ", s.unchanged),
                unch_style.add_modifier(Modifier::DIM),
            )
        };
        vec![
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
            Span::styled(
                format!(" +{} ", s.added),
                Style::default()
                    .fg(COLOR_ADDED)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" -{} ", s.removed),
                Style::default()
                    .fg(COLOR_REMOVED)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" ~{} ", s.modified),
                Style::default()
                    .fg(COLOR_MODIFIED)
                    .add_modifier(Modifier::BOLD),
            ),
            unchanged_badge,
        ]
    };

    let header = Paragraph::new(Line::from(left_spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .style(Style::default().bg(COLOR_PANEL)),
    );

    f.render_widget(header, area);
}

fn draw_file_list(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::FileList;
    let border_color = if focused { COLOR_BORDER_FOCUSED } else { COLOR_BORDER };

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
            " Files ({}) … ",
            app.filtered_indices.len()
        )
    } else {
        let suffix = if !app.show_unchanged { " · changed only" } else { "" };
        format!(" Files ({}){}  ", app.filtered_indices.len(), suffix)
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
            let path_str = file.rel_path.to_string_lossy();
            let binary_flag = if file.is_binary { " [bin]" } else { "" };
            let text_color = if file.kind == ChangeKind::Unchanged {
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
                    format!("{}{}", path_str, binary_flag),
                    Style::default().fg(text_color),
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
    let border_color = if focused { COLOR_BORDER_FOCUSED } else { COLOR_BORDER };

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

    // While loading with nothing yet, show a hint
    let visible: Vec<Line> = if app.loading && app.filtered_indices.is_empty() {
        vec![Line::from(Span::styled(
            "  Scanning…",
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

    let paragraph = Paragraph::new(Text::from(visible))
        .block(
            Block::default()
                .title(file_title.as_str())
                .title_style(Style::default().fg(COLOR_TEXT).add_modifier(Modifier::BOLD))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(COLOR_PANEL)),
        )
        .wrap(Wrap { trim: false });

    f.render_widget(paragraph, area);

    if app.diff_lines.len() > inner_h {
        let mut sb = ScrollbarState::new(app.diff_lines.len()).position(app.diff_scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        let sb_area = Rect {
            x: area.right() - 1,
            y: area.top() + 1,
            width: 1,
            height: area.height - 2,
        };
        f.render_stateful_widget(scrollbar, sb_area, &mut sb);
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
                .unwrap_or_else(|| "     ".to_string());
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
                .unwrap_or_else(|| "     ".to_string());
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
                .unwrap_or_else(|| "     ".to_string());
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
            " Scanning… ({} files found so far)  q Quit",
            app.filtered_indices.len()
        )
    } else if app.focus == Focus::FileList {
        " ↑↓/jk Navigate  Enter/Tab → Diff  / Search  u Toggle Unchanged  ? Help  q Quit"
            .to_string()
    } else {
        " ↑↓/jk Scroll  PgUp/PgDn Fast  Esc/Tab → Files  ? Help  q Quit".to_string()
    };

    f.render_widget(
        Paragraph::new(msg.as_str())
            .style(Style::default().fg(COLOR_DIM).bg(Color::Rgb(12, 14, 20))),
        area,
    );
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let width = 56u16;
    let height = 24u16;
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let overlay = Rect::new(x, y, width, height);

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
        ("  ?", "This help"),
        ("  q / Ctrl+C", "Quit"),
        ("", ""),
        ("  Flags (CLI)", ""),
        ("  --gitignore", "Respect .gitignore"),
        ("  -I '*.lock'", "Ignore glob pattern"),
        ("  --plain", "Non-TUI output"),
        ("  --all / -a", "Show unchanged in plain"),
        ("", ""),
        ("  Press any key to close", ""),
    ];

    let text: Vec<Line> = rows
        .iter()
        .map(|(left, right)| {
            if right.is_empty() && !left.is_empty() && !left.starts_with("  Press") {
                Line::from(Span::styled(
                    left.to_string(),
                    Style::default()
                        .fg(COLOR_HEADER)
                        .add_modifier(Modifier::UNDERLINED),
                ))
            } else if right.is_empty() {
                Line::from(Span::styled(
                    left.to_string(),
                    Style::default()
                        .fg(COLOR_DIM)
                        .add_modifier(Modifier::ITALIC),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("{:<22}", left), Style::default().fg(COLOR_MODIFIED)),
                    Span::styled(right.to_string(), Style::default().fg(COLOR_TEXT)),
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


