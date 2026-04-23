//! Mesh chat TUI.
//!
//! Modes:
//! - Main     : chat (channels + messages + input)
//! - Settings : displays LoRa config + role (key `s`)
//! - Channels : detailed list of the 8 channels (key `c`)
//!
//! Global keybindings:
//! - Tab / Shift-Tab : next / previous channel
//! - Enter           : send message
//! - PgUp / PgDn     : scroll messages
//! - `s`             : settings modal
//! - `c`             : channels modal
//! - Esc             : close modal / quit when on Main with empty input
//! - Ctrl+C          : quit from anywhere

use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;

use anyhow::{bail, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use mesh_core::{
    ChannelInfo, ChannelPrivacy, ChannelRole, ChatMessage, LoraInfo, MeshBackend, MeshCommand,
    MeshEvent, Network, NodeInfo, SendStatus,
};
use mesh_lib::config::{load_config, Config, NetworkChoice};
use mesh_lib::history::{
    detect_history_format, dump_history_to_stdout, history_file_path, init_new_v2, load_history,
    rotate_if_needed, unlock_v2, DetectedFormat, HistoryMode, HistoryWriter,
};
use mesh_lib::serial::available_ports;
use meshcore_backend::MeshcoreBackend;
use meshtastic_backend::MeshtasticBackend;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Row, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Table, Wrap,
    },
    Terminal,
};

// Shared palette — referenced everywhere instead of hardcoding Color::X values.
mod palette {
    use ratatui::style::Color;
    pub const ACCENT: Color = Color::Yellow; // selection, unread, highlight
    pub const INFO: Color = Color::Cyan; // other users, info
    pub const ME: Color = Color::Green; // my messages
    pub const ERR: Color = Color::Red;
    pub const DIM: Color = Color::DarkGray; // timestamps, hints, disabled
    pub const PRIMARY: Color = Color::Green;
    pub const SECONDARY: Color = Color::Cyan;
}
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

/// Best-effort send on the backend command channel. Logs at warn when
/// the channel is closed (typically means the backend already exited,
/// so the UI is about to die anyway) rather than silently dropping.
async fn dispatch(commands: &mpsc::Sender<MeshCommand>, cmd: MeshCommand) {
    if let Err(e) = commands.send(cmd).await {
        warn!(error = %e, "backend command channel closed; command dropped");
    }
}

const CHANNEL_COUNT: u32 = 8;

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum Mode {
    #[default]
    Main,
    Settings,
    Channels,
    ChannelEditor,
    ChannelDelete,
    Nodes,
    UserEditor,
}

/// A "space" the user can be reading / writing in. Channels and DM threads
/// are interchangeable from the UI's perspective — you just Tab between
/// them like in Slack.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Space {
    Channel(u32),
    Dm(String),
}

impl Space {
    fn channel_idx(&self) -> Option<u32> {
        match self {
            Space::Channel(i) => Some(*i),
            _ => None,
        }
    }
}

impl Default for Space {
    fn default() -> Self {
        Space::Channel(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UserField {
    LongName,
    ShortName,
}

/// Form state for editing the device's long/short name.
struct UserEdit {
    long_name: String,
    short_name: String,
    active_field: UserField,
    /// If true, show the confirm prompt. `y` commits, any other key returns
    /// to editing.
    confirming: bool,
}

/// Meshtastic caps — see `User` protobuf.
const MAX_LONG_NAME: usize = 39;
const MAX_SHORT_NAME: usize = 4;

/// PSK preset the user can pick in the channel editor. Each preset maps to a
/// specific raw-byte encoding when the command is sent to the radio.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PskChoice {
    None,
    Default,
    Default2,
    Default3,
    Default4,
    Default5,
    Default6,
    Default7,
    Default8,
    Default9,
    Default10,
    Random16,
    Random32,
}

impl PskChoice {
    const ALL: [PskChoice; 13] = [
        PskChoice::None,
        PskChoice::Default,
        PskChoice::Default2,
        PskChoice::Default3,
        PskChoice::Default4,
        PskChoice::Default5,
        PskChoice::Default6,
        PskChoice::Default7,
        PskChoice::Default8,
        PskChoice::Default9,
        PskChoice::Default10,
        PskChoice::Random16,
        PskChoice::Random32,
    ];

    fn label(&self) -> &'static str {
        match self {
            PskChoice::None => "none (no crypto)",
            PskChoice::Default => "default (LongFast key)",
            PskChoice::Default2 => "default2",
            PskChoice::Default3 => "default3",
            PskChoice::Default4 => "default4",
            PskChoice::Default5 => "default5",
            PskChoice::Default6 => "default6",
            PskChoice::Default7 => "default7",
            PskChoice::Default8 => "default8",
            PskChoice::Default9 => "default9",
            PskChoice::Default10 => "default10",
            PskChoice::Random16 => "random16 (AES128)",
            PskChoice::Random32 => "random32 (AES256)",
        }
    }

    fn generate(&self) -> Vec<u8> {
        use rand::RngExt;
        match self {
            PskChoice::None => Vec::new(),
            PskChoice::Default => vec![1],
            PskChoice::Default2 => vec![2],
            PskChoice::Default3 => vec![3],
            PskChoice::Default4 => vec![4],
            PskChoice::Default5 => vec![5],
            PskChoice::Default6 => vec![6],
            PskChoice::Default7 => vec![7],
            PskChoice::Default8 => vec![8],
            PskChoice::Default9 => vec![9],
            PskChoice::Default10 => vec![10],
            PskChoice::Random16 => {
                let mut b = vec![0u8; 16];
                rand::rng().fill(&mut b[..]);
                b
            }
            PskChoice::Random32 => {
                let mut b = vec![0u8; 32];
                rand::rng().fill(&mut b[..]);
                b
            }
        }
    }

    /// Best-effort round-trip from an existing PSK (we can't distinguish a
    /// random 16-byte key from a typed one — assume Random16/Random32 for
    /// those lengths).
    fn from_psk(psk: &[u8]) -> Self {
        match psk.len() {
            0 => PskChoice::None,
            1 => match psk[0] {
                1 => PskChoice::Default,
                2 => PskChoice::Default2,
                3 => PskChoice::Default3,
                4 => PskChoice::Default4,
                5 => PskChoice::Default5,
                6 => PskChoice::Default6,
                7 => PskChoice::Default7,
                8 => PskChoice::Default8,
                9 => PskChoice::Default9,
                10 => PskChoice::Default10,
                _ => PskChoice::Default,
            },
            32 => PskChoice::Random32,
            _ => PskChoice::Random16,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorField {
    Name,
    Psk,
}

struct ChannelEdit {
    index: u32,
    is_new: bool,
    name: String,
    psk_choice: PskChoice,
    active_field: EditorField,
    /// When true, show the confirm prompt. `y` commits, any other key returns
    /// to editing.
    confirming: bool,
    /// Generated PSK bytes (populated when entering the confirm phase so the
    /// preview matches exactly what will be written).
    pending_psk: Option<Vec<u8>>,
}

#[derive(Default)]
struct AppState {
    messages: Vec<Vec<ChatMessage>>,
    unread: Vec<usize>,
    nodes: HashMap<String, NodeInfo>,
    channels: Vec<Option<ChannelInfo>>,
    lora: Option<LoraInfo>,
    role: Option<String>,
    /// The space (channel or DM thread) the user is currently reading /
    /// writing in. Unified so Tab naturally cycles between everything.
    current_space: Space,
    input: String,
    my_id: Option<String>,
    scroll: u16,
    status: String,
    mode: Mode,
    /// Cursor position inside the channels modal (0..CHANNEL_COUNT).
    channels_sel_idx: u32,
    editor: Option<ChannelEdit>,
    user_editor: Option<UserEdit>,
    /// DM threads keyed by the peer's node id (the "other" end).
    dm_threads: HashMap<String, Vec<ChatMessage>>,
    /// Unread counter per DM thread.
    dm_unread: HashMap<String, usize>,
    /// Cursor position inside the nodes modal (into the sorted view).
    nodes_sel_idx: usize,
    /// Index being deleted, when mode == ChannelDelete.
    delete_index: Option<u32>,
    /// Transient flash message shown in the channels modal footer.
    channels_flash: Option<String>,
    /// Channel index we just wrote to — cleared when the radio echoes back
    /// the matching ChannelInfo (success) or reports an error (failure).
    pending_channel_write: Option<u32>,
    /// Monotonic counter used to tag each outgoing text message so we can
    /// correlate a `SendResult` event back with the UI message.
    next_local_id: u64,
    /// Append-only history writer. None = persistence disabled.
    history: Option<HistoryWriter>,
    /// True if history is persisted encrypted on disk.
    history_encrypted: bool,
    /// Number of messages successfully restored from history at startup.
    history_restored: usize,
    /// Number of lines that could not be decrypted / parsed at load time.
    history_load_errors: usize,
}

impl AppState {
    fn new(mode: HistoryMode) -> Self {
        let history_encrypted = mode.is_encrypted();
        let mut s = Self {
            messages: (0..CHANNEL_COUNT).map(|_| Vec::new()).collect(),
            unread: (0..CHANNEL_COUNT).map(|_| 0).collect(),
            channels: (0..CHANNEL_COUNT).map(|_| None).collect(),
            current_space: Space::Channel(0),
            status: "connecting…".to_string(),
            history: Some(HistoryWriter::open(mode.clone())),
            history_encrypted,
            ..Default::default()
        };
        s.load_history(&mode);
        s
    }

    fn load_history(&mut self, mode: &HistoryMode) {
        let channel_count = self.messages.len();
        let messages = &mut self.messages;
        let report = load_history(mode, |msg| {
            let idx = (msg.channel as usize).min(channel_count.saturating_sub(1));
            messages[idx].push(msg);
        });
        self.history_restored = report.restored;
        self.history_load_errors = report.errors;
    }

    fn push_message(&mut self, msg: ChatMessage) {
        let idx = (msg.channel as usize).min(self.messages.len().saturating_sub(1));
        let is_current = self.current_space == Space::Channel(idx as u32);
        let is_me = self
            .my_id
            .as_deref()
            .map(|id| id == msg.from)
            .unwrap_or(false);
        if let Some(h) = self.history.as_mut() {
            h.record(&msg);
        }
        self.messages[idx].push(msg);
        if !is_current && !is_me {
            self.unread[idx] += 1;
        }
    }

    fn push_dm(&mut self, peer: String, msg: ChatMessage) {
        let is_current = self.current_space == Space::Dm(peer.clone());
        let is_me = self
            .my_id
            .as_deref()
            .map(|id| id == msg.from)
            .unwrap_or(false);
        if let Some(h) = self.history.as_mut() {
            h.record(&msg);
        }
        self.dm_threads.entry(peer.clone()).or_default().push(msg);
        if !is_current && !is_me {
            *self.dm_unread.entry(peer).or_insert(0) += 1;
        }
    }

    /// Routes an incoming text message either into a regular channel bucket
    /// or into a DM thread when the destination matches our node id.
    fn route_incoming(&mut self, msg: ChatMessage) {
        let is_dm = match self.my_id.as_deref() {
            Some(id) => msg.to == id,
            None => false,
        };
        if is_dm {
            let peer = msg.from.clone();
            self.push_dm(peer, msg);
        } else {
            self.push_message(msg);
        }
    }

    /// Switches the current space, resets scroll, clears unread for that
    /// space, and (for DMs) guarantees the thread exists.
    fn switch_space(&mut self, space: Space) {
        self.scroll = 0;
        match &space {
            Space::Channel(i) => {
                if let Some(u) = self.unread.get_mut(*i as usize) {
                    *u = 0;
                }
            }
            Space::Dm(peer) => {
                self.dm_threads.entry(peer.clone()).or_default();
                self.dm_unread.remove(peer);
            }
        }
        self.current_space = space;
    }

    fn current_messages(&self) -> &[ChatMessage] {
        match &self.current_space {
            Space::Channel(i) => self
                .messages
                .get(*i as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
            Space::Dm(peer) => self
                .dm_threads
                .get(peer)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
        }
    }

    fn display_name(&self, id: &str) -> String {
        self.nodes
            .get(id)
            .map(|n| n.long_name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.to_string())
    }

    fn channel_label(&self, idx: u32) -> String {
        match self.channels.get(idx as usize).and_then(|c| c.as_ref()) {
            Some(c) if !c.name.is_empty() => c.name.clone(),
            Some(c) if c.role == ChannelRole::Primary => "default".to_string(),
            _ => format!("ch{}", idx),
        }
    }

    fn channel_privacy(&self, idx: u32) -> Option<ChannelPrivacy> {
        self.channels
            .get(idx as usize)
            .and_then(|c| c.as_ref())
            .filter(|c| c.role != ChannelRole::Disabled)
            .map(|c| c.privacy())
    }

    fn current_privacy(&self) -> Option<ChannelPrivacy> {
        match &self.current_space {
            Space::Channel(i) => self.channel_privacy(*i),
            // DMs are end-to-end encrypted by the firmware when both peers
            // have PKC keys — treat as private in the UI.
            Space::Dm(_) => Some(ChannelPrivacy::Private),
        }
    }

    fn current_label(&self) -> String {
        match &self.current_space {
            Space::Channel(i) => self.channel_label(*i),
            Space::Dm(peer) => self.display_name(peer),
        }
    }

    /// Ordered list of spaces the user can Tab through: enabled channels
    /// first (index order), then DM threads (most recent first).
    fn spaces_in_order(&self) -> Vec<Space> {
        let mut out: Vec<Space> = (0..CHANNEL_COUNT)
            .filter(|i| {
                self.channels
                    .get(*i as usize)
                    .and_then(|c| c.as_ref())
                    .map(|c| c.role != ChannelRole::Disabled)
                    .unwrap_or(true)
            })
            .map(Space::Channel)
            .collect();
        for peer in dm_thread_order(self) {
            out.push(Space::Dm(peer));
        }
        out
    }

    fn cycle_space(&mut self, forward: bool) {
        let spaces = self.spaces_in_order();
        if spaces.is_empty() {
            return;
        }
        let pos = spaces
            .iter()
            .position(|s| s == &self.current_space)
            .unwrap_or(0);
        let len = spaces.len();
        let next = if forward {
            pos.wrapping_add(1) % len
        } else {
            (pos + len - 1) % len
        };
        if let Some(space) = spaces.get(next).cloned() {
            self.switch_space(space);
        }
    }
}

fn privacy_color(p: ChannelPrivacy) -> Color {
    match p {
        ChannelPrivacy::Private => palette::ME, // green — safe
        ChannelPrivacy::Public => palette::ERR, // red — readable by anyone
    }
}

fn privacy_marker(p: ChannelPrivacy) -> &'static str {
    match p {
        ChannelPrivacy::Private => "●",
        ChannelPrivacy::Public => "○",
    }
}

fn privacy_label(p: ChannelPrivacy) -> &'static str {
    match p {
        ChannelPrivacy::Private => " PRIVATE ",
        ChannelPrivacy::Public => " PUBLIC ",
    }
}

enum UiEvent {
    /// Boxed to keep the enum small — `MeshEvent` is ~224 bytes because of
    /// variants like `Reaction` and `Lora`/`DeviceRole` info; `KeyEvent` and
    /// `Tick` would otherwise pay that full cost too.
    Mesh(Box<MeshEvent>),
    Key(KeyEvent),
    Tick,
}

/// Determines the history mode, prompting the user for a passphrase when
/// needed. Called before the TUI takes over the terminal so stdin is still
/// the normal shell.
fn resolve_mode_interactively(config: &Config) -> Result<HistoryMode> {
    // Rotate first — so the magic header of a new encrypted file matches
    // the passphrase we're about to set up, instead of being carried over
    // from a bloated old file.
    if let Some(max) = config.history.max_size_mb {
        if let Err(e) = rotate_if_needed(max) {
            eprintln!("Warning: history rotation failed: {}", e);
        }
    }
    if !config.history.encrypt {
        return Ok(HistoryMode::Plaintext);
    }

    let path = history_file_path()
        .ok_or_else(|| anyhow::anyhow!("no data dir available for history"))?;
    match detect_history_format(&path) {
        DetectedFormat::Empty => {
            eprintln!("╭─ mesh-chat · set history passphrase ───────────────╮");
            eprintln!("│ Choose a passphrase to encrypt history.jsonl.      │");
            eprintln!("│ It is never written to disk — you'll be asked     │");
            eprintln!("│ for it at every launch. Losing it = losing the    │");
            eprintln!("│ history.                                           │");
            eprintln!("╰────────────────────────────────────────────────────╯");
            let p1 = rpassword::prompt_password("New passphrase: ")?;
            if p1.is_empty() {
                bail!("empty passphrase refused");
            }
            let p2 = rpassword::prompt_password("Confirm passphrase: ")?;
            if p1 != p2 {
                bail!("passphrases do not match");
            }
            init_new_v2(&p1)
        }
        DetectedFormat::V2 { salt } => {
            let p = rpassword::prompt_password("history passphrase: ")?;
            unlock_v2(salt, &p)
        }
        DetectedFormat::V1Legacy => bail!(
            "History file is in the legacy v1 format (pre-passphrase). \
             Move it aside or run an older build to migrate."
        ),
        DetectedFormat::Plaintext => bail!(
            "History file is plaintext but [history] encrypt = true. \
             Either disable encryption in config, or move the file aside."
        ),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = load_config();
    let args: Vec<String> = std::env::args().collect();

    // One-shot: decrypt history to stdout and exit.
    if args.iter().any(|a| a == "--dump-history") {
        let mode = resolve_mode_interactively(&config)?;
        return dump_history_to_stdout(&mode);
    }

    // Log to file — stdout is taken over by the TUI. Default path follows
    // `std::env::temp_dir()` so it works on Linux, macOS and Windows.
    let log_dir = std::env::var("MESH_LOG_DIR")
        .ok()
        .or_else(|| config.general.log_dir.clone())
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let file_appender = tracing_appender::rolling::never(&log_dir, "mesh-chat-tui.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| mesh_lib::logging::DEFAULT_FILTER.into()),
        )
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

    info!(log_dir = %log_dir, "tui start");

    let history_mode = resolve_mode_interactively(&config)?;
    info!(
        encrypt = history_mode.is_encrypted(),
        "history mode resolved"
    );

    let port = match resolve_port(&config)? {
        Some(p) => p,
        None => return Ok(()), // user aborted picker
    };
    info!(port = %port, "port selected");

    let backend: Box<dyn MeshBackend> = match config.general.network {
        NetworkChoice::Meshtastic => Box::new(MeshtasticBackend::new(port.clone())),
        NetworkChoice::Meshcore => Box::new(MeshcoreBackend::new(port.clone())),
    };
    info!(network = ?config.general.network, "backend selected from config");
    let handle = match backend.start().await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Failed to start backend on {}: {}", port, e);
            return Err(e);
        }
    };

    run_ui(handle, history_mode).await
}

/// Resolves the serial port to use:
/// 1. CLI arg `--port X`
/// 2. env var `MESH_PORT`
/// 3. config file (`general.port`)
/// 4. scan + interactive picker
fn resolve_port(config: &Config) -> Result<Option<String>> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--port" || a == "-p") {
        if let Some(port) = args.get(pos + 1) {
            return Ok(Some(port.clone()));
        }
        bail!("--port expects an argument");
    }
    for a in &args {
        if let Some(rest) = a.strip_prefix("--port=") {
            return Ok(Some(rest.to_string()));
        }
    }

    if let Ok(port) = std::env::var("MESH_PORT") {
        if !port.is_empty() {
            return Ok(Some(port));
        }
    }

    if let Some(port) = config.general.port.as_ref() {
        if !port.is_empty() {
            return Ok(Some(port.clone()));
        }
    }

    let ports = available_ports()?;
    match ports.len() {
        0 => {
            eprintln!("No serial port detected in /dev.");
            eprintln!("Plug your radio (ttyACM* or ttyUSB*) and try again.");
            eprintln!("Or force a port: `--port /dev/ttyACM0`.");
            Ok(None)
        }
        1 => {
            println!("Detected port: {}", ports[0]);
            Ok(Some(ports[0].clone()))
        }
        _ => pick_port_interactive(&ports),
    }
}

fn pick_port_interactive(ports: &[String]) -> Result<Option<String>> {
    println!("Multiple serial ports detected:");
    for (i, p) in ports.iter().enumerate() {
        println!("  [{}] {}", i, p);
    }
    print!("Pick [0-{}] (q to cancel): ", ports.len() - 1);
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed == "q" || trimmed.is_empty() {
        return Ok(None);
    }
    let idx: usize = trimmed
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid input: {:?}", trimmed))?;
    ports
        .get(idx)
        .cloned()
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("index out of range: {}", idx))
}

async fn run_ui(handle: mesh_core::BackendHandle, history_mode: HistoryMode) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_event_loop(&mut terminal, handle, history_mode).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    handle: mesh_core::BackendHandle,
    history_mode: HistoryMode,
) -> Result<()> {
    let mesh_core::BackendHandle {
        mut events,
        commands,
    } = handle;

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(256);

    let tx_mesh = ui_tx.clone();
    tokio::spawn(async move {
        while let Some(evt) = events.recv().await {
            if tx_mesh.send(UiEvent::Mesh(Box::new(evt))).await.is_err() {
                break;
            }
        }
        debug!("mesh forward task terminated");
    });

    let tx_key = ui_tx.clone();
    std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => {
                if let Ok(Event::Key(key)) = event::read() {
                    if tx_key.blocking_send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
            }
            Ok(false) => {
                if tx_key.blocking_send(UiEvent::Tick).is_err() {
                    break;
                }
            }
            Err(e) => {
                warn!(error = %e, "crossterm poll error");
                break;
            }
        }
    });

    let mut state = AppState::new(history_mode);
    terminal.draw(|f| draw(f, &state))?;

    while let Some(evt) = ui_rx.recv().await {
        match evt {
            UiEvent::Mesh(mesh_evt) => apply_mesh_event(&mut state, *mesh_evt),
            UiEvent::Key(key) => {
                if handle_key(&mut state, key, &commands).await == ControlFlow::Quit {
                    dispatch(&commands, MeshCommand::Shutdown).await;
                    break;
                }
            }
            UiEvent::Tick => {}
        }
        terminal.draw(|f| draw(f, &state))?;
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
enum ControlFlow {
    Continue,
    Quit,
}

fn apply_mesh_event(state: &mut AppState, evt: MeshEvent) {
    match evt {
        MeshEvent::Connected { network, my_id } => {
            state.my_id = Some(my_id.clone());
            state.status = format!("connected · {} · {}", network.as_str(), my_id);
            info!(my_id = %my_id, "connected");
        }
        MeshEvent::Disconnected { network } => {
            state.status = format!("disconnected · {}", network.as_str());
            warn!(network = network.as_str(), "disconnected");
        }
        MeshEvent::TextMessage(msg) => {
            debug!(channel = msg.channel, from = %msg.from, to = %msg.to, "message received");
            state.route_incoming(msg);
        }
        MeshEvent::NodeSeen(info) => {
            state.nodes.insert(info.id.clone(), info);
        }
        MeshEvent::ChannelInfo(info) => {
            let idx = info.index as usize;
            let matches_pending = state.pending_channel_write == Some(info.index);
            if matches_pending {
                state.pending_channel_write = None;
                state.channels_flash = Some(format!(
                    "channel #{} confirmed by radio · role={} name={} psk={}",
                    info.index,
                    info.role.as_str(),
                    if info.name.is_empty() {
                        "—"
                    } else {
                        info.name.as_str()
                    },
                    info.psk_display()
                ));
            }
            if idx < state.channels.len() {
                state.channels[idx] = Some(info);
            }
        }
        MeshEvent::LoraInfo(info) => {
            state.lora = Some(info);
        }
        MeshEvent::DeviceRoleInfo(info) => {
            state.role = Some(info.role);
        }
        MeshEvent::ConfigComplete { .. } => {
            if state.status.starts_with("connecting") {
                state.status = "ready".to_string();
            }
        }
        MeshEvent::SendResult {
            local_id,
            ok,
            error,
            ..
        } => {
            let new_status = if ok {
                SendStatus::Sent
            } else {
                SendStatus::Failed(error.clone().unwrap_or_else(|| "unknown".into()))
            };
            update_outgoing_status(state, local_id, new_status);
        }
        MeshEvent::SendAck {
            local_id,
            delivered,
            error,
            ..
        } => {
            // Routing ack: promote Sent → Delivered, or downgrade to Failed
            // with the firmware's error code (MAX_RETRANSMIT, NO_CHANNEL, …).
            // Do not overwrite an already-Failed status from SendResult —
            // that was a hard local failure.
            let new_status = if delivered {
                SendStatus::Delivered
            } else {
                SendStatus::Failed(
                    error
                        .clone()
                        .unwrap_or_else(|| "routing failure".to_string()),
                )
            };
            update_outgoing_status(state, local_id, new_status);
        }
        MeshEvent::Reaction {
            reply_to_packet_id,
            emoji,
            from,
            ..
        } => {
            // The ratatui TUI doesn't render reactions inline yet (it would
            // need packet_id tracking on every displayed message + a pill
            // row in the bubble layout). Log the event so the user knows it
            // was heard, but don't modify the chat buffer.
            debug!(
                reply_to_packet_id,
                from = %from,
                emoji = %emoji,
                "reaction ignored by TUI (display not implemented)"
            );
        }
        MeshEvent::Position {
            from,
            latitude,
            longitude,
            ..
        } => {
            // Same story as Reaction — the TUI doesn't have a sidebar slot
            // for peer positions yet. Log for discoverability.
            debug!(
                from = %from,
                latitude,
                longitude,
                "position ignored by TUI (display not implemented)"
            );
        }
        MeshEvent::Telemetry {
            from,
            battery_level,
            voltage,
            ..
        } => {
            // The TUI's existing NodeSeen handler already stores
            // battery/voltage; dedicated telemetry history is a GUI-only
            // feature for now.
            debug!(
                from = %from,
                battery = ?battery_level,
                voltage = ?voltage,
                "telemetry ignored by TUI"
            );
        }
        MeshEvent::Error { network, message } => {
            state.status = format!("error · {} · {}", network.as_str(), message);
            warn!(network = network.as_str(), %message, "mesh error");
            // If we were waiting on a channel write, surface the error in the
            // channels modal flash rather than only the header.
            if let Some(idx) = state.pending_channel_write.take() {
                state.channels_flash = Some(format!("channel #{} write failed: {}", idx, message));
            }
        }
    }
}

/// Update the `status` of the outgoing message with this `local_id`,
/// searching both channel buffers and DM threads. `Delivered` never
/// overwrites a terminal `Failed` status (e.g. a local serial error that
/// happened before the radio had a chance to route).
fn update_outgoing_status(state: &mut AppState, local_id: u64, new_status: SendStatus) {
    let promote = |m: &mut ChatMessage| {
        let can_overwrite = !matches!(&m.status, Some(SendStatus::Failed(_)))
            || !matches!(new_status, SendStatus::Delivered);
        if can_overwrite {
            m.status = Some(new_status.clone());
        }
    };
    for chan_msgs in state.messages.iter_mut() {
        if let Some(m) = chan_msgs.iter_mut().find(|m| m.local_id == Some(local_id)) {
            promote(m);
            return;
        }
    }
    for dm_msgs in state.dm_threads.values_mut() {
        if let Some(m) = dm_msgs.iter_mut().find(|m| m.local_id == Some(local_id)) {
            promote(m);
            return;
        }
    }
}

async fn handle_key(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::Sender<MeshCommand>,
) -> ControlFlow {
    // Ctrl+C always wins.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return ControlFlow::Quit;
    }

    match state.mode {
        Mode::Main => handle_key_main(state, key, commands).await,
        Mode::Settings => handle_key_settings(state, key),
        Mode::Nodes => handle_key_nodes(state, key),
        Mode::Channels => handle_key_channels(state, key),
        Mode::ChannelEditor => handle_key_editor(state, key, commands).await,
        Mode::ChannelDelete => handle_key_delete(state, key, commands).await,
        Mode::UserEditor => handle_key_user_edit(state, key, commands).await,
    }
}

fn handle_key_nodes(state: &mut AppState, key: KeyEvent) -> ControlFlow {
    let sorted = sorted_node_ids(state);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.mode = Mode::Main;
        }
        KeyCode::Up if !sorted.is_empty() => {
            state.nodes_sel_idx = (state.nodes_sel_idx + sorted.len() - 1) % sorted.len();
        }
        KeyCode::Down if !sorted.is_empty() => {
            state.nodes_sel_idx = (state.nodes_sel_idx + 1) % sorted.len();
        }
        KeyCode::Enter => {
            if let Some(peer) = sorted.get(state.nodes_sel_idx).cloned() {
                if state.my_id.as_deref() == Some(peer.as_str()) {
                    // no self-DMs
                    return ControlFlow::Continue;
                }
                state.switch_space(Space::Dm(peer));
                state.mode = Mode::Main;
            }
        }
        _ => {}
    }
    ControlFlow::Continue
}

/// Returns node ids in the same order the nodes modal displays them.
fn sorted_node_ids(state: &AppState) -> Vec<String> {
    let mut v: Vec<&NodeInfo> = state.nodes.values().collect();
    v.sort_by(|a, b| {
        b.last_heard
            .cmp(&a.last_heard)
            .then_with(|| a.long_name.cmp(&b.long_name))
    });
    v.into_iter().map(|n| n.id.clone()).collect()
}

/// DM threads sorted by most recent activity first (peer id as tie-break).
fn dm_thread_order(state: &AppState) -> Vec<String> {
    let mut v: Vec<(&String, i64)> = state
        .dm_threads
        .iter()
        .map(|(peer, msgs)| {
            let last_ts = msgs.last().map(|m| m.timestamp).unwrap_or(0);
            (peer, last_ts)
        })
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    v.into_iter().map(|(p, _)| p.clone()).collect()
}

fn handle_key_settings(state: &mut AppState, key: KeyEvent) -> ControlFlow {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.mode = Mode::Main;
        }
        KeyCode::Char('e') => {
            // Prefill from the current node's existing User info if known.
            let (long_name, short_name) = match state.my_id.as_deref() {
                Some(id) => match state.nodes.get(id) {
                    Some(n) => (n.long_name.clone(), n.short_name.clone()),
                    None => (String::new(), String::new()),
                },
                None => (String::new(), String::new()),
            };
            state.user_editor = Some(UserEdit {
                long_name,
                short_name,
                active_field: UserField::LongName,
                confirming: false,
            });
            state.mode = Mode::UserEditor;
        }
        _ => {}
    }
    ControlFlow::Continue
}

async fn handle_key_user_edit(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::Sender<MeshCommand>,
) -> ControlFlow {
    let Some(edit) = state.user_editor.as_mut() else {
        state.mode = Mode::Settings;
        return ControlFlow::Continue;
    };

    if edit.confirming {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let long_name = edit.long_name.clone();
                let short_name = edit.short_name.clone();
                info!(%long_name, %short_name, "sending UpdateUser");
                dispatch(
                    commands,
                    MeshCommand::UpdateUser {
                        long_name: long_name.clone(),
                        short_name: short_name.clone(),
                    },
                )
                .await;
                // Optimistic local update so the header "me (X)" refreshes.
                if let Some(id) = state.my_id.clone() {
                    state
                        .nodes
                        .entry(id.clone())
                        .and_modify(|n| {
                            n.long_name = long_name.clone();
                            n.short_name = short_name.clone();
                        })
                        .or_insert_with(|| NodeInfo {
                            network: Network::Meshtastic,
                            id,
                            long_name,
                            short_name,
                            battery_level: None,
                            voltage: None,
                            snr: None,
                            last_heard: None,
                            hops_away: None,
                        });
                }
                state.user_editor = None;
                state.mode = Mode::Settings;
            }
            _ => {
                edit.confirming = false;
            }
        }
        return ControlFlow::Continue;
    }

    match (key.code, edit.active_field) {
        (KeyCode::Esc, _) => {
            state.user_editor = None;
            state.mode = Mode::Settings;
        }
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
            edit.active_field = match edit.active_field {
                UserField::LongName => UserField::ShortName,
                UserField::ShortName => UserField::LongName,
            };
        }
        (KeyCode::Enter, _) => {
            if edit.long_name.trim().is_empty() {
                return ControlFlow::Continue;
            }
            if edit.short_name.trim().is_empty() {
                return ControlFlow::Continue;
            }
            edit.confirming = true;
        }
        (KeyCode::Backspace, UserField::LongName) => {
            edit.long_name.pop();
        }
        (KeyCode::Backspace, UserField::ShortName) => {
            edit.short_name.pop();
        }
        (KeyCode::Char(c), UserField::LongName)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && edit.long_name.len() < MAX_LONG_NAME =>
        {
            edit.long_name.push(c);
        }
        (KeyCode::Char(c), UserField::ShortName)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && edit.short_name.len() < MAX_SHORT_NAME =>
        {
            edit.short_name.push(c);
        }
        _ => {}
    }
    ControlFlow::Continue
}

fn handle_key_channels(state: &mut AppState, key: KeyEvent) -> ControlFlow {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            state.mode = Mode::Main;
            state.channels_flash = None;
        }
        KeyCode::Up => {
            state.channels_sel_idx = (state.channels_sel_idx + CHANNEL_COUNT - 1) % CHANNEL_COUNT;
            state.channels_flash = None;
        }
        KeyCode::Down => {
            state.channels_sel_idx = (state.channels_sel_idx + 1) % CHANNEL_COUNT;
            state.channels_flash = None;
        }
        KeyCode::Char('n') => {
            // New channel: find first Disabled slot (index >= 1, primary is ch0).
            let slot = (1..CHANNEL_COUNT).find(|i| {
                matches!(
                    state
                        .channels
                        .get(*i as usize)
                        .and_then(|c| c.as_ref())
                        .map(|c| c.role),
                    Some(ChannelRole::Disabled) | None
                )
            });
            match slot {
                Some(idx) => open_editor(state, idx, /* is_new */ true),
                None => {
                    state.channels_flash = Some("no empty slot (all 8 channels in use)".into());
                }
            }
        }
        KeyCode::Char('e') => {
            let idx = state.channels_sel_idx;
            if idx == 0 {
                state.channels_flash = Some("primary channel is read-only".into());
            } else {
                let role = state
                    .channels
                    .get(idx as usize)
                    .and_then(|c| c.as_ref())
                    .map(|c| c.role);
                if matches!(role, Some(ChannelRole::Primary)) {
                    state.channels_flash = Some("primary channel is read-only".into());
                } else {
                    open_editor(state, idx, /* is_new */ false);
                }
            }
        }
        KeyCode::Char('d') => {
            let idx = state.channels_sel_idx;
            if idx == 0 {
                state.channels_flash = Some("primary channel is read-only".into());
            } else {
                let role = state
                    .channels
                    .get(idx as usize)
                    .and_then(|c| c.as_ref())
                    .map(|c| c.role);
                match role {
                    Some(ChannelRole::Secondary) => {
                        state.delete_index = Some(idx);
                        state.mode = Mode::ChannelDelete;
                    }
                    Some(ChannelRole::Primary) => {
                        state.channels_flash = Some("primary channel is read-only".into());
                    }
                    _ => {
                        state.channels_flash = Some("channel already disabled".into());
                    }
                }
            }
        }
        _ => {}
    }
    ControlFlow::Continue
}

fn open_editor(state: &mut AppState, idx: u32, is_new: bool) {
    let existing = state.channels.get(idx as usize).and_then(|c| c.as_ref());
    let (name, psk_choice) = if is_new {
        (String::new(), PskChoice::Random16)
    } else {
        match existing {
            // Editing an empty slot behaves like "new" for PSK default.
            Some(c) if matches!(c.role, ChannelRole::Disabled) => {
                (c.name.clone(), PskChoice::Random16)
            }
            Some(c) => (c.name.clone(), PskChoice::from_psk(&c.psk)),
            None => (String::new(), PskChoice::Random16),
        }
    };
    state.editor = Some(ChannelEdit {
        index: idx,
        is_new,
        name,
        psk_choice,
        active_field: EditorField::Name,
        confirming: false,
        pending_psk: None,
    });
    state.mode = Mode::ChannelEditor;
    state.channels_flash = None;
}

async fn handle_key_editor(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::Sender<MeshCommand>,
) -> ControlFlow {
    let Some(edit) = state.editor.as_mut() else {
        state.mode = Mode::Channels;
        return ControlFlow::Continue;
    };

    if edit.confirming {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // Commit: take the pending PSK (already generated) and send.
                let psk = edit
                    .pending_psk
                    .clone()
                    .unwrap_or_else(|| edit.psk_choice.generate());
                let index = edit.index;
                let name = edit.name.clone();
                info!(index, psk_len = psk.len(), "sending SetChannel");
                dispatch(
                    commands,
                    MeshCommand::SetChannel {
                        index,
                        role: ChannelRole::Secondary,
                        name: name.clone(),
                        psk: psk.clone(),
                    },
                )
                .await;
                // Optimistic local update — the radio doesn't always echo back
                // a ChannelInfo after SetChannel, so refresh the list now.
                if let Some(slot) = state.channels.get_mut(index as usize) {
                    *slot = Some(ChannelInfo {
                        network: Network::Meshtastic,
                        index,
                        role: ChannelRole::Secondary,
                        name,
                        psk,
                        uplink_enabled: true,
                        downlink_enabled: true,
                    });
                }
                state.pending_channel_write = Some(index);
                state.editor = None;
                state.mode = Mode::Channels;
                state.channels_flash = Some(format!("channel #{} saved", index));
            }
            _ => {
                // Any other key cancels the confirmation — back to editing.
                edit.confirming = false;
                edit.pending_psk = None;
            }
        }
        return ControlFlow::Continue;
    }

    match (key.code, edit.active_field) {
        (KeyCode::Esc, _) => {
            state.editor = None;
            state.mode = Mode::Channels;
        }
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
            edit.active_field = match edit.active_field {
                EditorField::Name => EditorField::Psk,
                EditorField::Psk => EditorField::Name,
            };
        }
        (KeyCode::Enter, _) => {
            // Validate + enter confirming state.
            if edit.name.trim().is_empty() {
                state.channels_flash = Some("name cannot be empty".into());
                return ControlFlow::Continue;
            }
            if edit.name.len() > 11 {
                state.channels_flash = Some("name too long (max 11 chars)".into());
                return ControlFlow::Continue;
            }
            edit.pending_psk = Some(edit.psk_choice.generate());
            edit.confirming = true;
        }
        (KeyCode::Backspace, EditorField::Name) => {
            edit.name.pop();
        }
        (KeyCode::Char(c), EditorField::Name)
            if !key.modifiers.contains(KeyModifiers::CONTROL) && edit.name.len() < 11 =>
        {
            edit.name.push(c);
        }
        (KeyCode::Left, EditorField::Psk) => {
            let cur = PskChoice::ALL
                .iter()
                .position(|p| *p == edit.psk_choice)
                .unwrap_or(0);
            let next = (cur + PskChoice::ALL.len() - 1) % PskChoice::ALL.len();
            edit.psk_choice = PskChoice::ALL[next];
        }
        (KeyCode::Right, EditorField::Psk) => {
            let cur = PskChoice::ALL
                .iter()
                .position(|p| *p == edit.psk_choice)
                .unwrap_or(0);
            let next = (cur + 1) % PskChoice::ALL.len();
            edit.psk_choice = PskChoice::ALL[next];
        }
        _ => {}
    }
    ControlFlow::Continue
}

async fn handle_key_delete(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::Sender<MeshCommand>,
) -> ControlFlow {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Some(idx) = state.delete_index.take() {
                info!(index = idx, "sending SetChannel(disabled)");
                dispatch(
                    commands,
                    MeshCommand::SetChannel {
                        index: idx,
                        role: ChannelRole::Disabled,
                        name: String::new(),
                        psk: Vec::new(),
                    },
                )
                .await;
                // Optimistic local update — see editor commit.
                if let Some(slot) = state.channels.get_mut(idx as usize) {
                    *slot = Some(ChannelInfo {
                        network: Network::Meshtastic,
                        index: idx,
                        role: ChannelRole::Disabled,
                        name: String::new(),
                        psk: Vec::new(),
                        uplink_enabled: false,
                        downlink_enabled: false,
                    });
                }
                state.pending_channel_write = Some(idx);
                state.channels_flash = Some(format!("channel #{} deleted", idx));
            }
            state.mode = Mode::Channels;
        }
        _ => {
            state.delete_index = None;
            state.mode = Mode::Channels;
        }
    }
    ControlFlow::Continue
}

async fn handle_key_main(
    state: &mut AppState,
    key: KeyEvent,
    commands: &mpsc::Sender<MeshCommand>,
) -> ControlFlow {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) if state.input.is_empty() => ControlFlow::Quit,
        (KeyCode::Esc, _) => {
            state.input.clear();
            ControlFlow::Continue
        }
        (KeyCode::Enter, _) => {
            let text = std::mem::take(&mut state.input);
            if text.trim().is_empty() {
                return ControlFlow::Continue;
            }
            state.next_local_id = state.next_local_id.saturating_add(1);
            let local_id = state.next_local_id;
            // DMs send on channel 0 (primary) and target a specific node.
            // Channels send broadcast on the channel index.
            let (channel, to, echo_to) = match state.current_space.clone() {
                Space::Channel(i) => (i, None, "^all".to_string()),
                Space::Dm(peer) => (0, Some(peer.clone()), peer),
            };
            info!(
                channel,
                local_id,
                bytes = text.len(),
                dm = to.is_some(),
                "send request"
            );
            if let Err(e) = commands
                .send(MeshCommand::SendText {
                    local_id,
                    channel,
                    text: text.clone(),
                    to,
                })
                .await
            {
                warn!(error = %e, "send_text command rejected");
            }
            let msg = ChatMessage {
                timestamp: chrono::Utc::now().timestamp(),
                network: Network::Meshtastic,
                channel,
                from: state.my_id.clone().unwrap_or_else(|| "me".into()),
                to: echo_to,
                text,
                local_id: Some(local_id),
                status: Some(SendStatus::Sending),
                rx_snr: None,
                rx_rssi: None,
                reply_to_text: None,
                packet_id: None,
                reactions: std::collections::HashMap::new(),
            };
            match state.current_space.clone() {
                Space::Channel(_) => state.push_message(msg),
                Space::Dm(peer) => state.push_dm(peer, msg),
            }
            ControlFlow::Continue
        }
        (KeyCode::Tab, _) => {
            state.cycle_space(true);
            ControlFlow::Continue
        }
        (KeyCode::BackTab, _) => {
            state.cycle_space(false);
            ControlFlow::Continue
        }
        (KeyCode::PageUp, _) => {
            state.scroll = state.scroll.saturating_add(5);
            ControlFlow::Continue
        }
        (KeyCode::PageDown, _) => {
            state.scroll = state.scroll.saturating_sub(5);
            ControlFlow::Continue
        }
        (KeyCode::Backspace, _) => {
            state.input.pop();
            ControlFlow::Continue
        }
        // Modal shortcuts — only trigger when the input is empty, otherwise
        // the character is typed into the current message.
        (KeyCode::Char('s'), KeyModifiers::NONE) if state.input.is_empty() => {
            state.mode = Mode::Settings;
            ControlFlow::Continue
        }
        (KeyCode::Char('c'), KeyModifiers::NONE) if state.input.is_empty() => {
            state.mode = Mode::Channels;
            state.channels_sel_idx = state.current_space.channel_idx().unwrap_or(0);
            state.channels_flash = None;
            ControlFlow::Continue
        }
        (KeyCode::Char('n'), KeyModifiers::NONE) if state.input.is_empty() => {
            state.mode = Mode::Nodes;
            ControlFlow::Continue
        }
        // `d` jumps straight to the most recent DM thread (or does nothing
        // if there are no DMs yet — the empty state itself points the user
        // to the Nodes modal).
        (KeyCode::Char('d'), KeyModifiers::NONE) if state.input.is_empty() => {
            if let Some(peer) = dm_thread_order(state).into_iter().next() {
                state.switch_space(Space::Dm(peer));
            }
            ControlFlow::Continue
        }
        (KeyCode::Char(c), mods) if !mods.contains(KeyModifiers::CONTROL) => {
            state.input.push(c);
            ControlFlow::Continue
        }
        _ => ControlFlow::Continue,
    }
}

fn draw(f: &mut ratatui::Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(5),    // body
            Constraint::Length(3), // input
            Constraint::Length(1), // help bar
        ])
        .split(f.area());

    draw_header(f, chunks[0], state);
    draw_body(f, chunks[1], state);
    draw_input(f, chunks[2], state);
    draw_help_bar(f, chunks[3], state);

    match state.mode {
        Mode::Settings => draw_settings_modal(f, state),
        Mode::Channels => draw_channels_modal(f, state),
        Mode::Nodes => draw_nodes_modal(f, state),
        Mode::ChannelEditor => {
            draw_channels_modal(f, state);
            draw_editor_modal(f, state);
        }
        Mode::ChannelDelete => {
            draw_channels_modal(f, state);
            draw_delete_modal(f, state);
        }
        Mode::UserEditor => {
            draw_settings_modal(f, state);
            draw_user_editor_modal(f, state);
        }
        Mode::Main => {}
    }
}

fn draw_header(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(40)])
        .split(area);

    // Left: branding + node id + history lock indicator
    let id = state.my_id.as_deref().unwrap_or("…");
    let mut left_spans = vec![
        Span::styled(
            " mesh-chat ",
            Style::default()
                .fg(Color::Black)
                .bg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled("▸", Style::default().fg(palette::DIM)),
        Span::raw(" "),
        Span::styled(
            id,
            Style::default()
                .fg(palette::INFO)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if state.history_encrypted {
        left_spans.push(Span::raw(" "));
        let lock_style = if state.history_load_errors > 0 {
            Style::default()
                .fg(palette::ERR)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(palette::ME)
                .add_modifier(Modifier::BOLD)
        };
        left_spans.push(Span::styled("🔒", lock_style));
    }
    f.render_widget(Paragraph::new(Line::from(left_spans)), cols[0]);

    // Right: privacy badge + radio summary + status
    let mut right_spans: Vec<Span> = Vec::new();
    if let Some(p) = state.current_privacy() {
        let badge_color = privacy_color(p);
        let fg = Color::White;
        right_spans.push(Span::styled(
            privacy_label(p),
            Style::default()
                .fg(fg)
                .bg(badge_color)
                .add_modifier(Modifier::BOLD),
        ));
        right_spans.push(Span::raw("  "));
    }
    if let Some(l) = &state.lora {
        right_spans.push(Span::styled(
            format!("{}/{}", l.region, l.modem_preset),
            Style::default().fg(palette::DIM),
        ));
        right_spans.push(Span::raw("  "));
    }
    let status_color =
        if state.status.starts_with("error") || state.status.starts_with("disconnected") {
            palette::ERR
        } else {
            palette::DIM
        };
    right_spans.push(Span::styled(
        &state.status,
        Style::default().fg(status_color),
    ));
    right_spans.push(Span::raw(" "));
    f.render_widget(
        Paragraph::new(Line::from(right_spans)).alignment(ratatui::layout::Alignment::Right),
        cols[1],
    );
}

fn draw_body(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(22), Constraint::Min(10)])
        .split(area);

    draw_sidebar(f, cols[0], state);
    draw_messages(f, cols[1], state);
}

fn draw_sidebar(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    // Unified list: channels first, then a "DMs" separator, then DM
    // threads (most recent first). The same Tab key cycles across the
    // whole list.
    let mut items: Vec<ListItem> = Vec::new();
    items.push(ListItem::new(Line::from(Span::styled(
        " channels",
        Style::default()
            .fg(palette::DIM)
            .add_modifier(Modifier::BOLD),
    ))));
    for idx in 0..CHANNEL_COUNT {
        items.push(channel_list_item(idx, state));
    }

    let threads = dm_thread_order(state);
    items.push(ListItem::new(Line::from(Span::styled(
        " direct messages",
        Style::default()
            .fg(palette::DIM)
            .add_modifier(Modifier::BOLD),
    ))));
    if threads.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "   (open the nodes list with `n` to start one)",
            Style::default()
                .fg(palette::DIM)
                .add_modifier(Modifier::ITALIC),
        ))));
    } else {
        for peer in &threads {
            items.push(dm_list_item(peer, state));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::DIM))
        .title(Span::styled(
            " Spaces ",
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn dm_list_item<'a>(peer: &str, state: &AppState) -> ListItem<'a> {
    let selected = state.current_space == Space::Dm(peer.to_string());
    let name = state.display_name(peer);
    let name = truncate(&name, 13);
    let unread = state.dm_unread.get(peer).copied().unwrap_or(0);
    let name_style = if selected {
        Style::default()
            .fg(palette::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::raw(if selected { " ▸ " } else { "   " }),
        Span::styled("✉", Style::default().fg(palette::INFO)),
        Span::raw(" "),
        Span::styled(name, name_style),
    ];
    if unread > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("+{}", unread),
            Style::default()
                .fg(Color::Black)
                .bg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn channel_list_item<'a>(idx: u32, state: &AppState) -> ListItem<'a> {
    let selected = state.current_space == Space::Channel(idx);
    let unread = state.unread.get(idx as usize).copied().unwrap_or(0);
    let info = state.channels.get(idx as usize).and_then(|c| c.as_ref());
    let privacy = state.channel_privacy(idx);
    let is_disabled = matches!(info.map(|c| c.role), Some(ChannelRole::Disabled) | None);

    let (marker, marker_color) = match privacy {
        Some(p) => (privacy_marker(p), privacy_color(p)),
        None => ("·", palette::DIM),
    };

    let name = state.channel_label(idx);
    let name = truncate(&name, 13);

    let name_style = if selected {
        Style::default()
            .fg(palette::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else if is_disabled {
        Style::default().fg(palette::DIM)
    } else if matches!(info.map(|c| c.role), Some(ChannelRole::Primary)) {
        // Give Primary a subtle visual weight via bold, so the user knows
        // which channel is the radio base setting.
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let mut spans = vec![
        Span::raw(if selected { " ▸ " } else { "   " }),
        Span::styled(marker, Style::default().fg(marker_color)),
        Span::raw(" "),
        Span::styled(name, name_style),
    ];

    if unread > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("+{}", unread),
            Style::default()
                .fg(Color::Black)
                .bg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
    }

    ListItem::new(Line::from(spans))
}

fn draw_messages(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let msgs = state.current_messages();

    let mut lines: Vec<Line> = Vec::new();
    let mut last_date: Option<String> = None;
    for m in msgs.iter() {
        let dt = chrono::DateTime::from_timestamp(m.timestamp, 0);
        let date_str = dt
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        if Some(&date_str) != last_date.as_ref() {
            lines.push(date_separator_line(&date_str));
            last_date = Some(date_str);
        }
        lines.push(format_message_line(m, state));
    }

    let is_empty = lines.is_empty();
    if is_empty {
        let hint = match &state.current_space {
            Space::Channel(_) => " (no messages — type text and press Enter to send) ",
            Space::Dm(_) => " (empty DM thread — type a message and Enter to send privately) ",
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default()
                .fg(palette::DIM)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    let label = state.current_label();
    let privacy = state.current_privacy();
    let border_color = privacy.map(privacy_color).unwrap_or(palette::DIM);

    // Left part of the title: `✉ peer_name` for DMs, `#idx name` for channels.
    let mut title_spans: Vec<Span> = Vec::new();
    title_spans.push(Span::raw(" "));
    match &state.current_space {
        Space::Channel(i) => {
            title_spans.push(Span::styled(
                format!("#{}", i),
                Style::default().fg(palette::DIM),
            ));
            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled(
                label,
                Style::default()
                    .fg(palette::INFO)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        Space::Dm(_) => {
            title_spans.push(Span::styled(
                "✉",
                Style::default().fg(palette::INFO),
            ));
            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled(
                label,
                Style::default()
                    .fg(palette::INFO)
                    .add_modifier(Modifier::BOLD),
            ));
            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled(
                "DM",
                Style::default().fg(palette::DIM),
            ));
        }
    }
    if let Some(p) = privacy {
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(
            privacy_label(p),
            Style::default()
                .fg(Color::White)
                .bg(privacy_color(p))
                .add_modifier(Modifier::BOLD),
        ));
    }
    title_spans.push(Span::raw(" "));
    let title = Line::from(title_spans);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title);

    // Scroll semantics: state.scroll = number of lines scrolled up from the
    // bottom. 0 means newest visible. We clamp and convert to an offset from
    // the top for ratatui's `Paragraph::scroll`.
    let visible = area.height.saturating_sub(2) as usize; // minus borders
    let total = lines.len();
    let max_scroll = total.saturating_sub(visible);
    let scroll_up = (state.scroll as usize).min(max_scroll);
    let offset = max_scroll.saturating_sub(scroll_up);

    let messages = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((offset as u16, 0))
        .block(block);
    f.render_widget(messages, area);

    // Scrollbar on the right edge, only when content overflows.
    if !is_empty && total > visible {
        let mut sb_state = ScrollbarState::new(total).position(offset);
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(palette::DIM));
        f.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }
}

fn date_separator_line(date: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("─── ", Style::default().fg(palette::DIM)),
        Span::styled(
            date.to_string(),
            Style::default()
                .fg(palette::DIM)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ───", Style::default().fg(palette::DIM)),
    ])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn format_message_line<'a>(m: &'a ChatMessage, state: &AppState) -> Line<'a> {
    let time = chrono::DateTime::from_timestamp(m.timestamp, 0)
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default();
    let is_me = state
        .my_id
        .as_deref()
        .map(|id| id == m.from)
        .unwrap_or(false);

    let dim = Style::default().fg(palette::DIM);

    if is_me {
        // My messages: right-aligned, green edge on the right as a "bubble tail".
        let me_style = Style::default()
            .fg(palette::ME)
            .add_modifier(Modifier::BOLD);
        let text_style = Style::default().fg(palette::ME);
        let me_label = match state.my_id.as_deref() {
            Some(id) => match state.nodes.get(id) {
                Some(n) if !n.long_name.is_empty() => format!("me ({})", n.long_name),
                _ => "me".to_string(),
            },
            None => "me".to_string(),
        };
        let (status_glyph, status_style) = match &m.status {
            Some(SendStatus::Sending) => ("…", Style::default().fg(palette::DIM)),
            Some(SendStatus::Sent) => ("✓", Style::default().fg(palette::ME)),
            Some(SendStatus::Delivered) => (
                "✓✓",
                Style::default()
                    .fg(palette::ME)
                    .add_modifier(Modifier::BOLD),
            ),
            Some(SendStatus::Failed(_)) => (
                "✗",
                Style::default()
                    .fg(palette::ERR)
                    .add_modifier(Modifier::BOLD),
            ),
            None => (" ", Style::default()),
        };
        Line::from(vec![
            Span::styled(status_glyph, status_style),
            Span::raw(" "),
            Span::styled(m.text.clone(), text_style),
            Span::styled("  ", dim),
            Span::styled(time, dim),
            Span::styled(" · ", dim),
            Span::styled(me_label, me_style),
            Span::raw(" "),
            Span::styled("▕", me_style),
        ])
        .right_aligned()
    } else {
        // Received messages: left-aligned, cyan edge on the left.
        let from = state.display_name(&m.from);
        let from_style = Style::default()
            .fg(palette::INFO)
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![
            Span::styled("▎", from_style),
            Span::raw(" "),
            Span::styled(from, from_style),
            Span::styled(" · ", dim),
            Span::styled(time, dim),
        ];
        if let (Some(rssi), Some(snr)) = (m.rx_rssi, m.rx_snr) {
            spans.push(Span::styled(format!(" · {}dBm {:+.1}dB", rssi, snr), dim));
        } else if let Some(rssi) = m.rx_rssi {
            spans.push(Span::styled(format!(" · {}dBm", rssi), dim));
        } else if let Some(snr) = m.rx_snr {
            spans.push(Span::styled(format!(" · {:+.1}dB", snr), dim));
        }
        spans.push(Span::styled("  ", dim));
        spans.push(Span::raw(m.text.clone()));
        Line::from(spans).left_aligned()
    }
}

fn draw_input(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    // Border + title reflect where this message is about to go: a channel
    // (with its privacy level) or a DM (always green, always private).
    let title_text = match state.mode {
        Mode::Main => match &state.current_space {
            Space::Channel(_) => match state.current_privacy() {
                Some(ChannelPrivacy::Private) => " ✎ message · private channel ".to_string(),
                Some(ChannelPrivacy::Public) => {
                    " ⚠ message · PUBLIC channel — anyone can read ".to_string()
                }
                None => " ✎ message ".to_string(),
            },
            Space::Dm(peer) => {
                format!(" ✉ DM to {} · end-to-end encrypted ", state.display_name(peer))
            }
        },
        _ => " (modal open) ".to_string(),
    };
    let border_color = match state.mode {
        Mode::Main => state
            .current_privacy()
            .map(privacy_color)
            .unwrap_or(palette::DIM),
        _ => palette::DIM,
    };
    let title = title_text.as_str();
    let input = Paragraph::new(state.input.as_str()).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(input, area);

    if state.mode == Mode::Main {
        let cursor_x = area.x + 1 + state.input.chars().count() as u16;
        let cursor_y = area.y + 1;
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

fn draw_help_bar(f: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let binds: &[(&str, &str)] = match state.mode {
        Mode::Main => &[
            ("↵", "send"),
            ("Tab", "space ▶"),
            ("⇧Tab", "◀"),
            ("s", "settings"),
            ("c", "channels"),
            ("n", "nodes"),
            ("d", "jump to DM"),
            ("Esc", "quit"),
        ],
        Mode::Settings => &[("e", "edit name"), ("Esc/q", "close")],
        Mode::Nodes => &[("↵", "open DM with selected"), ("↑↓", "select"), ("Esc/q", "close")],
        Mode::UserEditor => &[("Tab", "field"), ("↵", "confirm"), ("Esc", "cancel")],
        Mode::Channels => &[
            ("↑↓", "select"),
            ("n", "new"),
            ("e", "edit"),
            ("d", "delete"),
            ("Esc/q", "close"),
        ],
        Mode::ChannelEditor => &[
            ("Tab", "field"),
            ("←→", "psk"),
            ("↵", "confirm"),
            ("Esc", "cancel"),
        ],
        Mode::ChannelDelete => &[("y", "confirm"), ("any", "cancel")],
    };

    // Total unread DMs across threads — shown as a badge next to the `d`
    // shortcut so the user sees they have new DMs waiting without opening
    // the view.
    let dm_unread_total: usize = state.dm_unread.values().sum();

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" "));
    for (i, (key, label)) in binds.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(palette::DIM)));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, Style::default().fg(palette::DIM)));
        // Unread badge next to the DMs shortcut in Main mode.
        if state.mode == Mode::Main && *key == "d" && dm_unread_total > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!(" +{} ", dm_unread_total),
                Style::default()
                    .fg(Color::Black)
                    .bg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn section_header(title: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("▸ ", Style::default().fg(palette::ACCENT)),
        Span::styled(
            title.to_string(),
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn kv_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {:<14}", label),
            Style::default().fg(palette::DIM),
        ),
        Span::styled(value, Style::default()),
    ])
}

fn draw_settings_modal(f: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(60, 70, f.area());
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![section_header("Node")];
    lines.push(kv_line(
        "id",
        state.my_id.clone().unwrap_or_else(|| "?".into()),
    ));
    lines.push(kv_line(
        "role",
        state.role.clone().unwrap_or_else(|| "?".into()),
    ));
    lines.push(Line::from(""));

    lines.push(section_header("LoRa"));
    if let Some(l) = &state.lora {
        lines.push(kv_line("region", l.region.clone()));
        lines.push(kv_line(
            "modem preset",
            format!(
                "{} ({})",
                l.modem_preset,
                if l.use_preset { "preset" } else { "manual" }
            ),
        ));
        lines.push(kv_line("hop limit", l.hop_limit.to_string()));
        lines.push(kv_line(
            "radio",
            format!(
                "BW {} kHz · SF {} · CR 4/{}",
                l.bandwidth, l.spread_factor, l.coding_rate
            ),
        ));
        lines.push(kv_line(
            "TX",
            format!(
                "{} dBm · {}",
                l.tx_power,
                if l.tx_enabled { "enabled" } else { "disabled" }
            ),
        ));
    } else {
        lines.push(Line::from(Span::styled(
            "  (not received yet)",
            Style::default().fg(palette::DIM),
        )));
    }

    lines.push(Line::from(""));
    lines.push(section_header("Channels"));
    let enabled = state
        .channels
        .iter()
        .filter(|c| {
            matches!(
                c.as_ref().map(|c| c.role),
                Some(ChannelRole::Primary) | Some(ChannelRole::Secondary)
            )
        })
        .count();
    lines.push(kv_line("active", format!("{}/8", enabled)));

    lines.push(Line::from(""));
    lines.push(section_header("History"));
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {:<14}", "encryption"),
            Style::default().fg(palette::DIM),
        ),
        if state.history_encrypted {
            Span::styled(
                "encrypted (ChaCha20-Poly1305) 🔒",
                Style::default()
                    .fg(palette::ME)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("plaintext", Style::default().fg(palette::DIM))
        },
    ]));
    if state.history_encrypted {
        lines.push(kv_line(
            "key source",
            "passphrase (Argon2id, in memory only)".to_string(),
        ));
    }
    lines.push(kv_line(
        "restored",
        format!("{} messages", state.history_restored),
    ));
    if state.history_load_errors > 0 {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<14}", "load errors"),
                Style::default().fg(palette::DIM),
            ),
            Span::styled(
                format!("{} lines skipped (see logs)", state.history_load_errors),
                Style::default()
                    .fg(palette::ERR)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    } else if state.history_restored > 0 {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<14}", "decryption"),
                Style::default().fg(palette::DIM),
            ),
            Span::styled("all lines OK ✓", Style::default().fg(palette::ME)),
        ]));
    }

    let widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette::ACCENT))
            .title(Span::styled(
                " ⚙  Settings ",
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(widget, area);
}

fn draw_channels_modal(f: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(78, 72, f.area());
    f.render_widget(Clear, area);

    // Reserve 1 line at the bottom inside the block for a flash message.
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(2)])
        .split(area);

    let header = Row::new(vec!["", "#", "role", "name", "PSK", "↑", "↓"])
        .style(
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let rows: Vec<Row> = (0..CHANNEL_COUNT)
        .map(|idx| {
            let is_sel = idx == state.channels_sel_idx;
            match state.channels.get(idx as usize).and_then(|c| c.as_ref()) {
                Some(c) => {
                    let (marker, marker_style) = match c.role {
                        ChannelRole::Primary => ("🔒", Style::default().fg(palette::PRIMARY)),
                        ChannelRole::Secondary => ("○", Style::default().fg(palette::SECONDARY)),
                        ChannelRole::Disabled => ("·", Style::default().fg(palette::DIM)),
                    };
                    let base_style = match c.role {
                        ChannelRole::Disabled => Style::default().fg(palette::DIM),
                        _ => Style::default(),
                    };
                    let row_style = if is_sel {
                        base_style.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                    } else {
                        base_style
                    };
                    Row::new(vec![
                        ratatui::text::Text::from(Span::styled(marker, marker_style)),
                        ratatui::text::Text::from(c.index.to_string()),
                        ratatui::text::Text::from(c.role.as_str().to_string()),
                        ratatui::text::Text::from(
                            if c.name.is_empty() && c.role == ChannelRole::Primary {
                                "default".into()
                            } else {
                                c.name.clone()
                            },
                        ),
                        ratatui::text::Text::from(c.psk_display()),
                        ratatui::text::Text::from(if c.uplink_enabled { "✓" } else { "·" }),
                        ratatui::text::Text::from(if c.downlink_enabled { "✓" } else { "·" }),
                    ])
                    .style(row_style)
                }
                None => {
                    let base = Style::default().fg(palette::DIM);
                    let row_style = if is_sel {
                        base.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                    } else {
                        base
                    };
                    Row::new(vec![
                        ratatui::text::Text::from("·"),
                        ratatui::text::Text::from(idx.to_string()),
                        ratatui::text::Text::from("—"),
                        ratatui::text::Text::from("(not received)"),
                        ratatui::text::Text::from("—"),
                        ratatui::text::Text::from("—"),
                        ratatui::text::Text::from("—"),
                    ])
                    .style(row_style)
                }
            }
        })
        .collect();

    let widths = [
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(10),
        Constraint::Min(14),
        Constraint::Length(20),
        Constraint::Length(2),
        Constraint::Length(2),
    ];

    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette::ACCENT))
            .title(Span::styled(
                " ☰  Channels ",
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(table, inner[0]);

    // Flash area (transient info, overlaps inside the block bottom padding).
    if let Some(flash) = &state.channels_flash {
        let flash_area = Rect {
            x: inner[1].x + 2,
            y: inner[1].y,
            width: inner[1].width.saturating_sub(4),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("⚑ {}", flash),
                Style::default()
                    .fg(palette::ERR)
                    .add_modifier(Modifier::BOLD),
            )),
            flash_area,
        );
    }
}

fn psk_preview(psk: &[u8]) -> String {
    match psk.len() {
        0 => "none".into(),
        1 => format!("default{}", psk[0]),
        n if n == 16 || n == 32 => {
            let hex: String = psk.iter().map(|b| format!("{:02x}", b)).collect();
            let head = &hex[..8];
            let tail = &hex[hex.len() - 8..];
            format!("{} bytes ({}…{})", n, head, tail)
        }
        n => format!("{} bytes", n),
    }
}

fn draw_editor_modal(f: &mut ratatui::Frame, state: &AppState) {
    let Some(edit) = state.editor.as_ref() else {
        return;
    };
    let area = centered_rect(55, 55, f.area());
    f.render_widget(Clear, area);

    let title = format!(
        " ✎  {} channel #{} ",
        if edit.is_new { "New" } else { "Edit" },
        edit.index
    );

    let mut lines: Vec<Line> = Vec::new();

    // Name field
    let name_focus = edit.active_field == EditorField::Name && !edit.confirming;
    let name_style = if name_focus {
        Style::default()
            .fg(palette::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let name_label = Style::default().fg(palette::DIM);
    lines.push(Line::from(vec![
        Span::styled(
            if name_focus { " ▸ " } else { "   " },
            Style::default().fg(palette::ACCENT),
        ),
        Span::styled("Name", name_label),
        Span::raw("  "),
        Span::styled("[", name_label),
        Span::styled(format!("{:<12}", edit.name), name_style),
        Span::styled("]", name_label),
        Span::styled(
            format!("  {}/11", edit.name.len()),
            Style::default().fg(palette::DIM),
        ),
    ]));

    // PSK field
    let psk_focus = edit.active_field == EditorField::Psk && !edit.confirming;
    let psk_style = if psk_focus {
        Style::default()
            .fg(palette::ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    lines.push(Line::from(vec![
        Span::styled(
            if psk_focus { " ▸ " } else { "   " },
            Style::default().fg(palette::ACCENT),
        ),
        Span::styled("PSK ", name_label),
        Span::raw(" "),
        Span::styled("‹", name_label),
        Span::raw(" "),
        Span::styled(edit.psk_choice.label(), psk_style),
        Span::raw(" "),
        Span::styled("›", name_label),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Role is always 'secondary' when creating from the UI.",
        Style::default().fg(palette::DIM),
    )));
    lines.push(Line::from(Span::styled(
        "  Primary channel (index 0) is read-only.",
        Style::default().fg(palette::DIM),
    )));

    if edit.confirming {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "─ Confirm ──────────────────────",
            Style::default().fg(palette::ACCENT),
        )));

        let prev = state
            .channels
            .get(edit.index as usize)
            .and_then(|c| c.as_ref());
        let old_role = prev.map(|c| c.role.as_str()).unwrap_or("disabled");
        let new_role = "secondary";
        let old_name = prev.map(|c| c.name.as_str()).unwrap_or("");
        let old_psk = prev.map(|c| c.psk.as_slice()).unwrap_or(&[]);
        let new_psk = edit.pending_psk.as_deref().unwrap_or(&[]);

        lines.push(Line::from(vec![
            Span::styled("  role : ", Style::default().fg(palette::DIM)),
            Span::raw(old_role.to_string()),
            Span::styled(" → ", Style::default().fg(palette::DIM)),
            Span::styled(
                new_role,
                Style::default()
                    .fg(palette::ME)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  name : ", Style::default().fg(palette::DIM)),
            Span::raw(if old_name.is_empty() {
                "(empty)".to_string()
            } else {
                old_name.to_string()
            }),
            Span::styled(" → ", Style::default().fg(palette::DIM)),
            Span::styled(
                if edit.name.is_empty() {
                    "(empty)".to_string()
                } else {
                    edit.name.clone()
                },
                Style::default()
                    .fg(palette::ME)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  PSK  : ", Style::default().fg(palette::DIM)),
            Span::raw(psk_preview(old_psk)),
            Span::styled(" → ", Style::default().fg(palette::DIM)),
            Span::styled(
                psk_preview(new_psk),
                Style::default()
                    .fg(palette::ME)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  Press ", Style::default().fg(palette::DIM)),
            Span::styled(
                "y",
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to write, any other key to keep editing",
                Style::default().fg(palette::DIM),
            ),
        ]));
    }

    let widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette::ACCENT))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(widget, area);
}

fn draw_user_editor_modal(f: &mut ratatui::Frame, state: &AppState) {
    let Some(edit) = state.user_editor.as_ref() else {
        return;
    };
    let area = centered_rect(55, 45, f.area());
    f.render_widget(Clear, area);

    let dim = Style::default().fg(palette::DIM);
    let long_focus = edit.active_field == UserField::LongName && !edit.confirming;
    let short_focus = edit.active_field == UserField::ShortName && !edit.confirming;
    let focus_style = |focused: bool| {
        if focused {
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                if long_focus { " ▸ " } else { "   " },
                Style::default().fg(palette::ACCENT),
            ),
            Span::styled("Long  ", dim),
            Span::styled("[", dim),
            Span::styled(format!("{:<39}", edit.long_name), focus_style(long_focus)),
            Span::styled("]", dim),
            Span::styled(format!("  {}/{}", edit.long_name.len(), MAX_LONG_NAME), dim),
        ]),
        Line::from(vec![
            Span::styled(
                if short_focus { " ▸ " } else { "   " },
                Style::default().fg(palette::ACCENT),
            ),
            Span::styled("Short ", dim),
            Span::styled("[", dim),
            Span::styled(format!("{:<4}", edit.short_name), focus_style(short_focus)),
            Span::styled("]", dim),
            Span::styled(
                format!("  {}/{}", edit.short_name.len(), MAX_SHORT_NAME),
                dim,
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Broadcast to the mesh via periodic NodeInfo packets.",
            dim,
        )),
    ];

    if edit.confirming {
        let prev_long = state
            .my_id
            .as_deref()
            .and_then(|id| state.nodes.get(id))
            .map(|n| n.long_name.clone())
            .unwrap_or_default();
        let prev_short = state
            .my_id
            .as_deref()
            .and_then(|id| state.nodes.get(id))
            .map(|n| n.short_name.clone())
            .unwrap_or_default();
        let highlight = Style::default()
            .fg(palette::ME)
            .add_modifier(Modifier::BOLD);
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "─ Confirm ──────────────────────",
            Style::default().fg(palette::ACCENT),
        )));
        lines.push(Line::from(vec![
            Span::styled("  long  : ", dim),
            Span::raw(if prev_long.is_empty() {
                "(empty)".into()
            } else {
                prev_long
            }),
            Span::styled(" → ", dim),
            Span::styled(edit.long_name.clone(), highlight),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  short : ", dim),
            Span::raw(if prev_short.is_empty() {
                "(empty)".into()
            } else {
                prev_short
            }),
            Span::styled(" → ", dim),
            Span::styled(edit.short_name.clone(), highlight),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  Press ", dim),
            Span::styled(
                "y",
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to write, any other key to keep editing", dim),
        ]));
    }

    let widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette::ACCENT))
            .title(Span::styled(
                " ✎  Edit node identity ",
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(widget, area);
}

fn format_relative_time(ts: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let delta = now - ts;
    match delta {
        d if d < 0 => "in the future".into(),
        d if d < 60 => format!("{}s", d),
        d if d < 3600 => format!("{}m", d / 60),
        d if d < 86400 => format!("{}h", d / 3600),
        d => format!("{}d", d / 86400),
    }
}

fn draw_nodes_modal(f: &mut ratatui::Frame, state: &AppState) {
    let area = centered_rect(80, 75, f.area());
    f.render_widget(Clear, area);

    // Sort nodes: most recent last_heard first, then name.
    let mut nodes: Vec<&NodeInfo> = state.nodes.values().collect();
    nodes.sort_by(|a, b| {
        b.last_heard
            .cmp(&a.last_heard)
            .then_with(|| a.long_name.cmp(&b.long_name))
    });

    let header = Row::new(vec!["id", "name", "batt", "snr", "hops", "seen"])
        .style(
            Style::default()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let rows: Vec<Row> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let is_me = state.my_id.as_deref() == Some(n.id.as_str());
            let is_sel = i == state.nodes_sel_idx;
            let batt = n
                .battery_level
                .map(|b| {
                    if b > 100 {
                        "⚡PWR".to_string()
                    } else {
                        format!("{}%", b)
                    }
                })
                .unwrap_or_else(|| "—".into());
            let snr = n
                .snr
                .map(|s| format!("{:+.1}dB", s))
                .unwrap_or_else(|| "—".into());
            let hops = n
                .hops_away
                .map(|h| h.to_string())
                .unwrap_or_else(|| "—".into());
            let seen = n
                .last_heard
                .map(format_relative_time)
                .unwrap_or_else(|| "—".into());
            let name = if n.long_name.is_empty() {
                "—".to_string()
            } else {
                n.long_name.clone()
            };
            let row = Row::new(vec![
                ratatui::text::Text::from(n.id.clone()),
                ratatui::text::Text::from(format!("{} {}", if is_me { "●" } else { " " }, name)),
                ratatui::text::Text::from(batt),
                ratatui::text::Text::from(snr),
                ratatui::text::Text::from(hops),
                ratatui::text::Text::from(seen),
            ]);
            let base = if is_me {
                Style::default()
                    .fg(palette::ME)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            if is_sel {
                row.style(base.bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            } else {
                row.style(base)
            }
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Min(14),
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(7),
    ];

    let title = format!(" ⧉  Nodes ({} seen) ", nodes.len());
    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette::ACCENT))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(table, area);
}

fn draw_delete_modal(f: &mut ratatui::Frame, state: &AppState) {
    let Some(idx) = state.delete_index else {
        return;
    };
    let area = centered_rect(50, 30, f.area());
    f.render_widget(Clear, area);

    let name = state
        .channels
        .get(idx as usize)
        .and_then(|c| c.as_ref())
        .map(|c| c.name.clone())
        .unwrap_or_default();

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Delete channel "),
            Span::styled(
                format!("#{}", idx),
                Style::default()
                    .fg(palette::ERR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!(
                    "({})",
                    if name.is_empty() {
                        "unnamed".into()
                    } else {
                        name
                    }
                ),
                Style::default().fg(palette::DIM),
            ),
            Span::raw("?"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  This sets the role to 'disabled' and clears the PSK.",
            Style::default().fg(palette::DIM),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Press ", Style::default().fg(palette::DIM)),
            Span::styled(
                "y",
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to confirm, any other key to cancel",
                Style::default().fg(palette::DIM),
            ),
        ]),
    ];

    let widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette::ERR))
            .title(Span::styled(
                " ✗  Delete channel ",
                Style::default()
                    .fg(palette::ERR)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(widget, area);
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use mesh_core::Network;

    fn outgoing_msg(local_id: u64, channel: u32) -> ChatMessage {
        ChatMessage {
            timestamp: 1,
            network: Network::Meshtastic,
            channel,
            from: "me".into(),
            to: "^all".into(),
            text: "hi".into(),
            local_id: Some(local_id),
            status: Some(SendStatus::Sending),
            rx_snr: None,
            rx_rssi: None,
            reply_to_text: None,
            packet_id: None,
            reactions: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn update_outgoing_status_promotes_channel_message() {
        let mut state = AppState {
            messages: (0..CHANNEL_COUNT).map(|_| Vec::new()).collect(),
            ..Default::default()
        };
        state.messages[2].push(outgoing_msg(42, 2));

        update_outgoing_status(&mut state, 42, SendStatus::Sent);
        assert_eq!(state.messages[2][0].status, Some(SendStatus::Sent));

        update_outgoing_status(&mut state, 42, SendStatus::Delivered);
        assert_eq!(state.messages[2][0].status, Some(SendStatus::Delivered));
    }

    #[test]
    fn update_outgoing_status_promotes_dm_message() {
        let mut state = AppState {
            messages: (0..CHANNEL_COUNT).map(|_| Vec::new()).collect(),
            ..Default::default()
        };
        state
            .dm_threads
            .entry("!beef".into())
            .or_default()
            .push(outgoing_msg(7, 0));

        update_outgoing_status(&mut state, 7, SendStatus::Delivered);
        let dm = &state.dm_threads["!beef"][0];
        assert_eq!(dm.status, Some(SendStatus::Delivered));
    }

    #[test]
    fn delivered_does_not_overwrite_failed() {
        // Local serial failure should stick even if a delayed routing ack
        // arrives afterwards for the same packet id (the radio may have
        // routed a stale copy, or we may be matching the wrong id).
        let mut state = AppState {
            messages: (0..CHANNEL_COUNT).map(|_| Vec::new()).collect(),
            ..Default::default()
        };
        state.messages[0].push(outgoing_msg(99, 0));
        update_outgoing_status(
            &mut state,
            99,
            SendStatus::Failed("serial dropped".into()),
        );
        update_outgoing_status(&mut state, 99, SendStatus::Delivered);
        assert!(matches!(
            state.messages[0][0].status,
            Some(SendStatus::Failed(_))
        ));
    }

    #[test]
    fn routing_failure_overwrites_sent() {
        // The radio accepted the packet (Sent) but the mesh couldn't deliver
        // it (NO_CHANNEL, MAX_RETRANSMIT, …). We want the error surfaced.
        let mut state = AppState {
            messages: (0..CHANNEL_COUNT).map(|_| Vec::new()).collect(),
            ..Default::default()
        };
        state.messages[0].push(outgoing_msg(5, 0));
        update_outgoing_status(&mut state, 5, SendStatus::Sent);
        update_outgoing_status(&mut state, 5, SendStatus::Failed("MAX_RETRANSMIT".into()));
        match &state.messages[0][0].status {
            Some(SendStatus::Failed(s)) => assert_eq!(s, "MAX_RETRANSMIT"),
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn update_outgoing_status_no_match_is_noop() {
        let mut state = AppState {
            messages: (0..CHANNEL_COUNT).map(|_| Vec::new()).collect(),
            ..Default::default()
        };
        // No panic, no matching message — function silently does nothing.
        update_outgoing_status(&mut state, 123, SendStatus::Delivered);
    }
}
