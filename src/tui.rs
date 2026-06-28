use std::{
    io::{self, Stdout},
    time::{Duration, SystemTime},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
};
use tokio::sync::oneshot;

use crate::{
    monitor::{ActiveRequest, CompletedRequest, MonitorHandle, MonitorState, SessionSummary},
    paths,
    registry::Registry,
};

pub struct MonitorUiConfig<'a> {
    pub port: u16,
    pub registry: &'a Registry,
    pub shutdown: Option<oneshot::Sender<()>>,
}

pub fn run_monitor(
    handle: MonitorHandle,
    config: MonitorUiConfig<'_>,
) -> Result<(), anyhow::Error> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = MonitorApp {
        port: config.port,
        setup_text: setup_text(config.port, config.registry),
        show_setup: true,
        show_help: false,
        detail: false,
        selected: 0,
        shutdown: config.shutdown,
    };

    loop {
        let state = handle.snapshot();
        terminal.draw(|frame| render(frame, &mut app, &state))?;
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('q') | KeyCode::Char('c')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        if let Some(shutdown) = app.shutdown.take() {
                            let _ = shutdown.send(());
                        }
                        break;
                    }
                    KeyCode::Char('?') => app.show_help = !app.show_help,
                    KeyCode::Char('b') => app.show_setup = !app.show_setup,
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.selected = app.selected.saturating_add(1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.selected = app.selected.saturating_sub(1)
                    }
                    KeyCode::Enter => app.detail = true,
                    KeyCode::Esc => app.detail = false,
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    terminal.show_cursor()?;
    Ok(())
}

struct MonitorApp {
    port: u16,
    setup_text: String,
    show_setup: bool,
    show_help: bool,
    detail: bool,
    selected: usize,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for MonitorApp {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, anyhow::Error> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn render(frame: &mut ratatui::Frame<'_>, app: &mut MonitorApp, state: &MonitorState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            if app.show_setup {
                Constraint::Length(9)
            } else {
                Constraint::Length(0)
            },
            Constraint::Percentage(40),
            Constraint::Percentage(25),
            Constraint::Percentage(35),
            Constraint::Length(if app.show_help { 5 } else { 3 }),
        ])
        .split(frame.area());

    render_header(frame, root[0], app, state);
    if app.show_setup {
        frame.render_widget(
            Paragraph::new(app.setup_text.as_str())
                .block(Block::default().title("Setup").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            root[1],
        );
    }
    if app.detail {
        render_session_detail(frame, root[2], state, app.selected);
    } else {
        render_sessions(frame, root[2], &state.sessions, app.selected);
    }
    render_active(frame, root[3], &state.active);
    render_recent(frame, root[4], &state.recent);
    render_footer(frame, root[5], app);
}

fn render_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &MonitorApp,
    state: &MonitorState,
) {
    let uptime = state
        .started_at
        .elapsed()
        .unwrap_or_else(|_| Duration::from_secs(0));
    let text = Line::from(vec![
        Span::styled(
            "claude-code-proxy",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  http://127.0.0.1:{}  uptime {}  sessions {}  active {}",
            app.port,
            format_duration(uptime),
            state.sessions.len(),
            state.active.len()
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_sessions(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    sessions: &[SessionSummary],
    selected: usize,
) {
    let rows = sessions.iter().enumerate().map(|(index, session)| {
        let marker = if index == selected { ">" } else { " " };
        Row::new(vec![
            Cell::from(marker),
            Cell::from(shorten(&session.label(), 26)),
            Cell::from(session.active_count.to_string()),
            Cell::from(session.request_count.to_string()),
            Cell::from(session.failure_count.to_string()),
            Cell::from(session.provider.as_deref().unwrap_or("-")),
            Cell::from(session.model.as_deref().unwrap_or("-")),
            Cell::from(format!(
                "{}/{}",
                session.input_tokens, session.output_tokens
            )),
            Cell::from(session.rate().label()),
            Cell::from(session.last_status.as_str()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Percentage(24),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(10),
            Constraint::Percentage(20),
            Constraint::Length(13),
            Constraint::Length(12),
            Constraint::Length(10),
        ],
    )
    .header(Row::new([
        "", "session", "active", "reqs", "fail", "provider", "model", "tokens", "rate", "status",
    ]))
    .block(Block::default().title("Sessions").borders(Borders::ALL));
    frame.render_widget(table, area);
}

fn render_active(frame: &mut ratatui::Frame<'_>, area: Rect, active: &[ActiveRequest]) {
    let rows = active.iter().map(|request| {
        Row::new(vec![
            Cell::from(format_system_time(request.started_at)),
            Cell::from(request.provider.as_deref().unwrap_or("-")),
            Cell::from(request.model.as_deref().unwrap_or("-")),
            Cell::from(request.endpoint.label()),
            Cell::from(request.status.label()),
            Cell::from(request.rate().label()),
            Cell::from(format_duration(request.elapsed())),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Percentage(24),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(9),
        ],
    )
    .header(Row::new([
        "started", "provider", "model", "endpoint", "status", "rate", "elapsed",
    ]))
    .block(
        Block::default()
            .title("Active requests")
            .borders(Borders::ALL),
    );
    frame.render_widget(table, area);
}

fn render_recent(frame: &mut ratatui::Frame<'_>, area: Rect, recent: &[CompletedRequest]) {
    let rows = recent.iter().map(|request| {
        let tokens = match (request.input_tokens, request.output_tokens) {
            (Some(input), Some(output)) => format!("{input}/{output}"),
            (Some(input), None) => input.to_string(),
            (None, Some(output)) => output.to_string(),
            (None, None) => "-".to_string(),
        };
        Row::new(vec![
            Cell::from(format_system_time(request.finished_at)),
            Cell::from(
                request
                    .http_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
            Cell::from(request.provider.as_deref().unwrap_or("-")),
            Cell::from(request.model.as_deref().unwrap_or("-")),
            Cell::from(format_duration(request.latency)),
            Cell::from(request.rate().label()),
            Cell::from(tokens),
            Cell::from(request.error.as_deref().unwrap_or("")),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Percentage(20),
            Constraint::Length(9),
            Constraint::Length(12),
            Constraint::Length(11),
            Constraint::Percentage(24),
        ],
    )
    .header(Row::new([
        "finished", "status", "provider", "model", "latency", "rate", "tokens", "error",
    ]))
    .block(
        Block::default()
            .title("Recent requests")
            .borders(Borders::ALL),
    );
    frame.render_widget(table, area);
}

fn render_session_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorState,
    selected: usize,
) {
    let lines = if let Some(session) = state.sessions.get(selected) {
        vec![
            Line::from(format!("session: {}", session.label())),
            Line::from(format!("active requests: {}", session.active_count)),
            Line::from(format!("total requests: {}", session.request_count)),
            Line::from(format!("failures: {}", session.failure_count)),
            Line::from(format!(
                "provider: {}",
                session.provider.as_deref().unwrap_or("-")
            )),
            Line::from(format!(
                "model: {}",
                session.model.as_deref().unwrap_or("-")
            )),
            Line::from(format!(
                "tokens: {}/{}",
                session.input_tokens, session.output_tokens
            )),
            Line::from(format!("rate: {}", session.rate().label())),
            Line::from(format!("last status: {}", session.last_status)),
        ]
    } else {
        vec![Line::from("No session selected")]
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title("Session detail")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &MonitorApp) {
    let hints = if app.show_help {
        "q/Ctrl-C quit  ? help  b setup  j/Down next  k/Up previous  Enter session detail  Esc back"
    } else {
        "q quit  ? help  b setup  Enter session"
    };
    frame.render_widget(
        Paragraph::new(hints).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

pub fn setup_text(port: u16, registry: &Registry) -> String {
    let mut lines = vec![
        format!("Logs: {}", paths::log_file().display()),
        format!("Config: {}", paths::config_dir().display()),
    ];
    for provider in ["codex", "kimi", "cursor"] {
        if let Some(models) = registry.grouped_models().get(provider) {
            lines.push(format!("{provider}: {}", models.join(", ")));
        }
    }
    lines.push(format!(
        "export ANTHROPIC_BASE_URL=\"http://localhost:{port}\""
    ));
    lines.push("export ANTHROPIC_AUTH_TOKEN=\"anything\"".to_string());
    lines.push("export ANTHROPIC_MODEL=\"gpt-5.5\"".to_string());
    lines.push("export ANTHROPIC_SMALL_FAST_MODEL=\"gpt-5.4-mini\"".to_string());
    lines.push("export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1".to_string());
    lines.join("\n")
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_system_time(time: SystemTime) -> String {
    let Ok(duration) = time.duration_since(SystemTime::UNIX_EPOCH) else {
        return "-".to_string();
    };
    let seconds = duration.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn shorten(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('~');
    out
}
