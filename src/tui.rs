use crate::diff::{ChangeKind, DiffLine, DiffResult, FileDiff, LineTag, get_file_diff_lines};
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

// ─── App state ────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum Focus {
    FileList,
    DiffView,
}

pub struct App {
    diff_result: DiffResult,
    list_state: ListState,
    diff_lines: Vec<DiffLine>,
    diff_scroll: usize,
    focus: Focus,
    /// When false (default), unchanged files are hidden
    show_unchanged: bool,
    search_query: String,
    search_mode: bool,
    /// Indices into diff_result.files that pass the current filter
    filtered_indices: Vec<usize>,
    status_msg: Option<String>,
    show_help: bool,
}

impl App {
    pub fn new(diff_result: DiffResult) -> Self {
        let mut app = App {
            diff_result,
            list_state: ListState::default(),
            diff_lines: Vec::new(),
            diff_scroll: 0,
            focus: Focus::FileList,
            show_unchanged: false, // ← changed-only by default
            search_query: String::new(),
            search_mode: false,
            filtered_indices: Vec::new(),
            status_msg: None,
            show_help: false,
        };
        app.rebuild_filter();
        if !app.filtered_indices.is_empty() {
            app.list_state.select(Some(0));
            app.load_diff(0);
        }
        app
    }

    fn rebuild_filter(&mut self) {
        let q = self.search_query.to_lowercase();
        self.filtered_indices = self
            .diff_result
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

    fn selected_file(&self) -> Option<&FileDiff> {
        let sel = self.list_state.selected()?;
        let idx = *self.filtered_indices.get(sel)?;
        self.diff_result.files.get(idx)
    }

    fn load_diff(&mut self, list_sel: usize) {
        if let Some(&idx) = self.filtered_indices.get(list_sel) {
            if let Some(file) = self.diff_result.files.get(idx) {
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
        self.load_diff(next);
    }

    fn scroll_diff(&mut self, delta: i32) {
        let max = self.diff_lines.len().saturating_sub(1);
        let cur = self.diff_scroll as i32;
        self.diff_scroll = (cur + delta).clamp(0, max as i32) as usize;
    }

    fn toggle_unchanged(&mut self) {
        // Remember which real file index we were on
        let cur_idx = self
            .list_state
            .selected()
            .and_then(|s| self.filtered_indices.get(s).copied());

        self.show_unchanged = !self.show_unchanged;
        self.rebuild_filter();

        // Try to keep selection on same file
        if let Some(old_idx) = cur_idx {
            if let Some(new_pos) = self.filtered_indices.iter().position(|&i| i == old_idx) {
                self.list_state.select(Some(new_pos));
            } else if !self.filtered_indices.is_empty() {
                self.list_state.select(Some(0));
                self.load_diff(0);
            }
        }

        let s = &self.diff_result.stats;
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

pub fn run_tui(diff_result: DiffResult) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(diff_result);

    loop {
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
                                app.load_diff(0);
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
                                app.load_diff(0);
                            }
                        }
                        KeyCode::Char(c) => {
                            app.search_query.push(c);
                            app.rebuild_filter();
                            if !app.filtered_indices.is_empty() {
                                app.list_state.select(Some(0));
                                app.load_diff(0);
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
                            app.load_diff(0);
                        } else {
                            app.diff_scroll = 0;
                        }
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        if app.focus == Focus::FileList {
                            let last = app.filtered_indices.len().saturating_sub(1);
                            app.list_state.select(Some(last));
                            app.load_diff(last);
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
    let s = &app.diff_result.stats;

    // Build badges: unchanged shown differently based on toggle
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

    let spans = vec![
        Span::styled(
            " 🍡 mochidetect ",
            Style::default()
                .fg(Color::Rgb(255, 200, 100))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", Style::default().fg(COLOR_BORDER)),
        Span::styled(
            &app.diff_result.old_path,
            Style::default().fg(Color::Rgb(180, 140, 220)),
        ),
        Span::styled("  →  ", Style::default().fg(COLOR_DIM)),
        Span::styled(
            &app.diff_result.new_path,
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
    ];

    let header = Paragraph::new(Line::from(spans)).block(
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
    } else {
        let suffix = if !app.show_unchanged {
            " · changed only"
        } else {
            ""
        };
        format!(" Files ({}){}  ", app.filtered_indices.len(), suffix)
    };

    let items: Vec<ListItem> = app
        .filtered_indices
        .iter()
        .map(|&idx| {
            let file = &app.diff_result.files[idx];
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
    let border_color = if focused {
        COLOR_BORDER_FOCUSED
    } else {
        COLOR_BORDER
    };

    let file_title = app
        .selected_file()
        .map(|fi| {
            let kind_color_label = match fi.kind {
                ChangeKind::Added => ("ADDED", COLOR_ADDED),
                ChangeKind::Removed => ("REMOVED", COLOR_REMOVED),
                ChangeKind::Modified => ("MODIFIED", COLOR_MODIFIED),
                ChangeKind::Unchanged => ("UNCHANGED", COLOR_UNCHANGED),
            };
            format!(
                " {} {} {} ",
                fi.kind.symbol(),
                fi.rel_path.to_string_lossy(),
                kind_color_label.0
            )
        })
        .unwrap_or_else(|| " Diff View ".to_string());

    let inner_h = area.height.saturating_sub(2) as usize;
    let visible: Vec<Line> = app
        .diff_lines
        .iter()
        .skip(app.diff_scroll)
        .take(inner_h)
        .map(render_diff_line)
        .collect();

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

    // Scrollbar
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
