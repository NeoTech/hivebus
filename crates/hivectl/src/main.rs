//! hivectl — TUI cluster inspector for subcluster hivebus.
//!
//! Usage:
//!   sc-hivectl                              # connect to default socket
//!   sc-hivectl /var/run/subcluster/hivebus.sock
//!
//! Keys:
//!   q / Ctrl-C   quit
//!   r            force refresh
//!   ↑ / ↓        select node row

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};

use proto::{
    decode_control, encode_control, ControlOp, ControlReply, NodeInfo, NodeState,
    CONTROL_FRAME_MARKER, MAX_CONTROL_PAYLOAD,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_SOCKET: &str = "/var/run/subcluster/hivebus.sock";
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct ClusterState {
    nodes: Vec<NodeInfo>,
    master_id: Option<u16>,
    queried_at: Instant,
    error: Option<String>,
}

struct App {
    cluster: ClusterState,
    table_state: TableState,
    socket_path: String,
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, Show);
    }
}

impl App {
    fn new(socket_path: String) -> Self {
        Self {
            cluster: ClusterState {
                nodes: vec![],
                master_id: None,
                // Set in the past so the first iteration triggers an immediate refresh.
                queried_at: Instant::now() - REFRESH_INTERVAL - Duration::from_millis(1),
                error: Some("connecting…".into()),
            },
            table_state: TableState::default(),
            socket_path,
        }
    }

    fn refresh(&mut self) {
        #[cfg(not(unix))]
        return;
        #[cfg(unix)]
        match query_hivebus(&self.socket_path) {
            Ok((nodes, master_id)) => {
                // Keep the selected row in bounds.
                if let Some(sel) = self.table_state.selected() {
                    if !nodes.is_empty() && sel >= nodes.len() {
                        self.table_state.select(Some(nodes.len() - 1));
                    }
                } else if !nodes.is_empty() {
                    self.table_state.select(Some(0));
                }
                self.cluster = ClusterState {
                    nodes,
                    master_id,
                    queried_at: Instant::now(),
                    error: None,
                };
            }
            Err(e) => {
                self.cluster.error = Some(format!("{:#}", e));
                self.cluster.queried_at = Instant::now();
            }
        }
    }

    fn scroll_up(&mut self) {
        let sel = self.table_state.selected().unwrap_or(0);
        if sel > 0 {
            self.table_state.select(Some(sel - 1));
        }
    }

    fn scroll_down(&mut self) {
        let len = self.cluster.nodes.len();
        if len == 0 {
            return;
        }
        let sel = self.table_state.selected().unwrap_or(0);
        if sel + 1 < len {
            self.table_state.select(Some(sel + 1));
        }
    }
}

// ---------------------------------------------------------------------------
// Socket communication
// ---------------------------------------------------------------------------

/// Send one `ControlOp` over a Unix socket and read back the `ControlReply`.
///
/// Framing (same as `proto::encode_control`):
///   op(1B) | payload_len(2B BE) | payload(bincode)
#[cfg(unix)]
fn send_op(stream: &mut UnixStream, op: &ControlOp) -> Result<ControlReply> {
    let bytes = encode_control(op).context("encode ControlOp")?;
    stream.write_all(&bytes).context("write to hivebus socket")?;

    // Read the 3-byte header.
    let mut hdr = [0u8; 3];
    stream
        .read_exact(&mut hdr)
        .context("read response header from hivebus")?;
    if hdr[0] != CONTROL_FRAME_MARKER {
        anyhow::bail!("invalid control frame marker: {}", hdr[0]);
    }
    let payload_len = u16::from_be_bytes([hdr[1], hdr[2]]) as usize;
    if payload_len > MAX_CONTROL_PAYLOAD {
        anyhow::bail!("control reply too large: {payload_len} bytes");
    }

    let mut payload = vec![0u8; payload_len];
    stream
        .read_exact(&mut payload)
        .context("read response payload from hivebus")?;

    decode_control::<ControlReply>(&payload).context("decode ControlReply")
}

/// Open two sequential connections to hivebus and query the full cluster view.
/// Two connections are used to keep the framing simple (one request per socket).
#[cfg(unix)]
fn query_hivebus(socket_path: &str) -> Result<(Vec<NodeInfo>, Option<u16>)> {
    // — GetNodes ————————————————————————————————————————————————————————
    let mut s1 = UnixStream::connect(socket_path)
        .with_context(|| format!("connect to {socket_path}"))?;
    s1.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    s1.set_write_timeout(Some(SOCKET_TIMEOUT))?;

    let nodes = match send_op(&mut s1, &ControlOp::GetNodes)? {
        ControlReply::Nodes(n) => n,
        ControlReply::Error { message } => {
            return Err(anyhow::anyhow!("hivebus GetNodes error: {message}"))
        }
        other => return Err(anyhow::anyhow!("unexpected GetNodes reply: {other:?}")),
    };

    // — GetMaster ——————————————————————————————————————————————————————
    let mut s2 = UnixStream::connect(socket_path)
        .with_context(|| format!("connect to {socket_path}"))?;
    s2.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    s2.set_write_timeout(Some(SOCKET_TIMEOUT))?;

    let master_id = match send_op(&mut s2, &ControlOp::GetMaster)? {
        ControlReply::Master(Some(n)) => Some(n.node_id),
        ControlReply::Master(None) => None,
        _ => None,
    };

    Ok((nodes, master_id))
}

// ---------------------------------------------------------------------------
// TUI rendering
// ---------------------------------------------------------------------------

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header bar
            Constraint::Min(5),    // node table
            Constraint::Length(1), // footer / key hints
        ])
        .split(area);

    draw_header(f, chunks[0], app);
    draw_table(f, chunks[1], app);
    draw_footer(f, chunks[2]);

    // Overlay error message on top of table area if there is one.
    if let Some(ref err) = app.cluster.error.clone() {
        draw_error(f, chunks[1], err);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let age = app.cluster.queried_at.elapsed();
    let age_str = if age.as_secs() < 60 {
        format!("{}s ago", age.as_secs())
    } else {
        format!("{}m{}s ago", age.as_secs() / 60, age.as_secs() % 60)
    };

    let alive = app
        .cluster
        .nodes
        .iter()
        .filter(|n| n.state == NodeState::Alive)
        .count();
    let total = app.cluster.nodes.len();

    let master_label = app
        .cluster
        .master_id
        .and_then(|id| app.cluster.nodes.iter().find(|n| n.node_id == id))
        .map(|n| format!("{} ({})", n.hostname, n.node_id))
        .unwrap_or_else(|| "—".to_string());

    let (conn_sym, conn_style) = if app.cluster.error.is_some() {
        ("✗", Style::default().fg(Color::Red))
    } else {
        ("✔", Style::default().fg(Color::Green))
    };

    let line = Line::from(vec![
        Span::styled(conn_sym, conn_style),
        Span::styled(
            format!("  socket: {}   ", app.socket_path),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("master: ", Style::default().fg(Color::Cyan)),
        Span::styled(
            master_label,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   nodes: {alive}/{total} alive"),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!("   updated {age_str}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let header = Paragraph::new(line)
        .block(Block::default().borders(Borders::ALL).title(" subcluster hivectl "));
    f.render_widget(header, area);
}

fn draw_table(f: &mut Frame, area: Rect, app: &mut App) {
    let header_cells = ["NODE ID", "HOSTNAME", "ADDRESS", "STATE", "★", "LAST SEEN"]
        .iter()
        .map(|h| {
            Cell::from(*h).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        });
    let header_row = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = app
        .cluster
        .nodes
        .iter()
        .map(|n| {
            let state_style = match n.state {
                NodeState::Alive => Style::default().fg(Color::Green),
                NodeState::Suspect => Style::default().fg(Color::Yellow),
                NodeState::Dead => Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::DIM),
            };
            let master_cell = if n.is_master {
                Cell::from("★")
                    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Cell::from("")
            };
            let last_seen = n
                .last_seen
                .elapsed()
                .map(|d| {
                    if d.as_secs() < 60 {
                        format!("{}s", d.as_secs())
                    } else {
                        format!("{}m{}s", d.as_secs() / 60, d.as_secs() % 60)
                    }
                })
                .unwrap_or_else(|_| "?".into());

            Row::new(vec![
                Cell::from(n.node_id.to_string()),
                Cell::from(n.hostname.clone()),
                Cell::from(n.control_addr.to_string()),
                Cell::from(format!("{:?}", n.state)).style(state_style),
                master_cell,
                Cell::from(last_seen),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(9),  // NODE ID
        Constraint::Length(16), // HOSTNAME
        Constraint::Length(22), // ADDRESS
        Constraint::Length(10), // STATE
        Constraint::Length(3),  // ★
        Constraint::Min(9),     // LAST SEEN
    ];

    let table = Table::new(rows, widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" nodes "))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let line = Paragraph::new(
        " [q] quit   [r] refresh   [↑ ↓] select   [ctrl-c] quit",
    )
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(line, area);
}

fn draw_error(f: &mut Frame, area: Rect, msg: &str) {
    let popup = centered_rect(70, 30, area);
    f.render_widget(Clear, popup);
    let para = Paragraph::new(format!("\n {msg}"))
        .style(Style::default().fg(Color::Red))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" connection error ")
                .border_style(Style::default().fg(Color::Red)),
        );
    f.render_widget(para, popup);
}

/// Return a rect centered within `r`, sized to `pct_w`% × `pct_h`%.
fn centered_rect(pct_w: u16, pct_h: u16, r: Rect) -> Rect {
    let margin_v = (100 - pct_h) / 2;
    let margin_h = (100 - pct_w) / 2;
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(margin_v),
            Constraint::Percentage(pct_h),
            Constraint::Percentage(margin_v),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(margin_h),
            Constraint::Percentage(pct_w),
            Constraint::Percentage(margin_h),
        ])
        .split(vert[1])[1]
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
fn main() {
    eprintln!("sc-hivectl is only supported on Unix/Linux");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() -> Result<()> {
    let socket_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_SOCKET.to_string());

    // Set up the terminal.
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let mut app = App::new(socket_path);
    let result = run_loop(&mut terminal, &mut app);

    // Always restore terminal, even on error.
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show).ok();
    terminal.show_cursor().ok();

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        // Poll for keyboard events with a short timeout so the auto-refresh
        // ticker fires roughly on schedule regardless of user input.
        if event::poll(POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(KeyEvent {
                    code: KeyCode::Char('q'),
                    ..
                }) => break,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => break,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('r'),
                    ..
                }) => app.refresh(),
                Event::Key(KeyEvent {
                    code: KeyCode::Up, ..
                }) => app.scroll_up(),
                Event::Key(KeyEvent {
                    code: KeyCode::Down,
                    ..
                }) => app.scroll_down(),
                _ => {}
            }
        }

        // Auto-refresh once per REFRESH_INTERVAL.
        if app.cluster.queried_at.elapsed() >= REFRESH_INTERVAL {
            app.refresh();
        }
    }
    Ok(())
}
