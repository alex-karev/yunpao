use crate::config::Global;
use crate::storage::ProjectState;
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use std::io::stdout;

pub enum TuiAction {
    Quit,
    SwitchSession(String),
    WatchLogs { session_id: String, action: String },
    None,
}

struct SessionInfo {
    id: String,
    name: String,
    server_label: String,
    is_current: bool,
    tasks: Vec<(String, String)>,
}

struct App {
    sessions: Vec<SessionInfo>,
    list_state: ListState,
}

impl App {
    fn new(config: &Global, state: &ProjectState) -> Self {
        let current_id = state.current_session().map(|s| s.id.clone());

        let mut sessions: Vec<SessionInfo> = state
            .list_sessions()
            .map(|session| {
                let server_label = config
                    .servers
                    .get(&session.server)
                    .and_then(|s| s.aliases.first().cloned())
                    .unwrap_or_else(|| session.server.clone());

                let mut tasks: Vec<(String, String)> = session
                    .tasks
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                tasks.sort_by(|a, b| a.0.cmp(&b.0));

                SessionInfo {
                    id: session.id.clone(),
                    name: session.name.clone(),
                    server_label,
                    is_current: current_id.as_deref() == Some(session.id.as_str()),
                    tasks,
                }
            })
            .collect();

        sessions.sort_by(|a, b| b.is_current.cmp(&a.is_current).then(a.name.cmp(&b.name)));

        let mut list_state = ListState::default();
        if !sessions.is_empty() {
            list_state.select(Some(0));
        }

        App { sessions, list_state }
    }

    fn selected(&self) -> Option<&SessionInfo> {
        self.list_state.selected().and_then(|i| self.sessions.get(i))
    }

    fn next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = self.list_state.selected().map_or(0, |i| (i + 1) % self.sessions.len());
        self.list_state.select(Some(i));
    }

    fn prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = self.list_state.selected().map_or(0, |i| {
            if i == 0 { self.sessions.len() - 1 } else { i - 1 }
        });
        self.list_state.select(Some(i));
    }
}

pub fn run(config: &Global, state: &ProjectState) -> Result<TuiAction> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(config, state);

    let action = loop {
        terminal.draw(|f| render(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break TuiAction::Quit,
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Up | KeyCode::Char('k') => app.prev(),
                    KeyCode::Enter | KeyCode::Char('s') => {
                        if let Some(s) = app.selected() {
                            if !s.is_current {
                                let id = s.id.clone();
                                break TuiAction::SwitchSession(id);
                            }
                        }
                    }
                    KeyCode::Char('l') => {
                        if let Some(s) = app.selected() {
                            if let Some((action, _)) = s.tasks.first() {
                                let session_id = s.id.clone();
                                let action = action.clone();
                                break TuiAction::WatchLogs { session_id, action };
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    };

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    Ok(action)
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(vertical[0]);

    render_sessions(frame, app, horizontal[0]);
    render_tasks(frame, app, horizontal[1]);
    render_help(frame, vertical[1]);
}

fn render_sessions(frame: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let (dot, style) = if s.is_current {
                ("● ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
            } else {
                ("○ ", Style::default().fg(Color::Gray))
            };
            let task_count = s.tasks.len();
            let suffix = if task_count > 0 {
                format!(" ({} running)", task_count)
            } else {
                String::new()
            };
            let label = format!("{}{} [{}]{}", dot, s.name, s.server_label, suffix);
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Sessions "))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_tasks(frame: &mut Frame, app: &mut App, area: Rect) {
    let content: Text = match app.selected() {
        None => Text::from(Span::styled("No session selected", Style::default().fg(Color::DarkGray))),
        Some(s) if s.tasks.is_empty() => {
            Text::from(Span::styled("No running tasks", Style::default().fg(Color::DarkGray)))
        }
        Some(s) => {
            let lines: Vec<Line> = s
                .tasks
                .iter()
                .map(|(name, pid)| {
                    Line::from(vec![
                        Span::styled("● ", Style::default().fg(Color::Yellow)),
                        Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(format!("  pid:{pid}")),
                    ])
                })
                .collect();
            Text::from(lines)
        }
    };

    let title = app
        .selected()
        .map(|s| format!(" Tasks — {} ", s.name))
        .unwrap_or_else(|| " Tasks ".to_string());

    let block = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(block, area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let help = Line::from(vec![
        Span::styled(" j/k↑↓ ", Style::default().fg(Color::Yellow)),
        Span::raw("navigate  "),
        Span::styled(" s/↵ ", Style::default().fg(Color::Yellow)),
        Span::raw("switch session  "),
        Span::styled(" l ", Style::default().fg(Color::Yellow)),
        Span::raw("watch logs  "),
        Span::styled(" q/Esc ", Style::default().fg(Color::Yellow)),
        Span::raw("quit"),
    ]);
    let block = Paragraph::new(help).block(Block::default().borders(Borders::ALL));
    frame.render_widget(block, area);
}
