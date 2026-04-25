//! Tauri desktop shell for mesh-chat. Wraps the same `MeshBackend` the TUI
//! uses, exposes its commands via `#[tauri::command]`, and forwards every
//! `MeshEvent` to the webview as a `mesh-event` event.
//!
//! History persistence and optional passphrase-derived encryption are
//! reused from `mesh-lib::history`. When encryption is enabled in the
//! config, the frontend must call `unlock_history(passphrase)` before
//! other commands that touch the writer (currently: `connect_device`).

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mesh_core::{ChannelRole, ChatMessage, MeshBackend, MeshCommand, MeshEvent, Network};
use mesh_lib::config::{load_config, NetworkChoice};
use mesh_lib::history::{
    delete_history_files, detect_history_format, history_file_path, init_new_v2, load_history,
    rotate_if_needed, unlock_v2, DetectedFormat, HistoryMode, HistoryWriter, LoadReport,
};
use mesh_lib::serial::available_ports;
use meshcore_backend::MeshcoreBackend;
use meshtastic_backend::MeshtasticBackend;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

/// Best-effort emit of a `mesh-event` to the webview. Unlike Tauri's own
/// `app.emit`, this logs at debug when the send fails (webview closing,
/// no listeners yet, etc.) instead of silently dropping the error.
fn emit_mesh_event(app: &AppHandle, evt: &MeshEvent) {
    if let Err(e) = app.emit("mesh-event", evt) {
        debug!(error = %e, "mesh-event emit failed");
    }
}

/// Shared state: the commands channel into the running backend (once
/// connected), the resolved history mode (None when an encrypted history is
/// still locked), the history writer once unlocked, and a monotonic counter
/// for outgoing-message local ids.
#[derive(Default)]
struct AppState {
    cmd_tx: Mutex<Option<mpsc::Sender<MeshCommand>>>,
    history_mode: Mutex<Option<HistoryMode>>,
    history: Mutex<Option<HistoryWriter>>,
    my_id: Mutex<Option<String>>,
    next_local_id: AtomicU64,
    /// Per-user overrides: alias map + favorites list. Loaded once at
    /// startup from `$XDG_DATA_HOME/mesh-chat/aliases.json`, rewritten
    /// atomically on every mutation.
    aliases: Mutex<mesh_lib::aliases::Aliases>,
    /// Which backend is actually running. The frontend reads this (via
    /// `get_network`) so it can e.g. grey out reaction buttons when on
    /// Meshcore where the companion protocol has no native reactions.
    current_network: Mutex<Option<Network>>,
}

#[derive(serde::Serialize)]
struct HistoryState {
    /// Config wants encryption.
    encrypt_requested: bool,
    /// Existing encrypted history file detected — user must enter the
    /// *existing* passphrase.
    existing_encrypted: bool,
    /// No history yet but encryption is requested — user must *set* a new
    /// passphrase.
    needs_setup: bool,
    /// A plaintext history file exists under an encrypt=true config; user
    /// must migrate or disable.
    has_legacy_plaintext: bool,
    /// A v1 legacy (key-file) history exists; no automatic migration path.
    has_legacy_v1: bool,
    /// True once the history is usable (plaintext mode or successfully
    /// unlocked).
    unlocked: bool,
}

fn detect_state_for(encrypt: bool) -> HistoryState {
    if !encrypt {
        return HistoryState {
            encrypt_requested: false,
            existing_encrypted: false,
            needs_setup: false,
            has_legacy_plaintext: false,
            has_legacy_v1: false,
            unlocked: true,
        };
    }
    let fmt = history_file_path()
        .map(|p| detect_history_format(&p))
        .unwrap_or(DetectedFormat::Empty);
    HistoryState {
        encrypt_requested: true,
        existing_encrypted: matches!(fmt, DetectedFormat::V2 { .. }),
        needs_setup: matches!(fmt, DetectedFormat::Empty),
        has_legacy_plaintext: matches!(fmt, DetectedFormat::Plaintext),
        has_legacy_v1: matches!(fmt, DetectedFormat::V1Legacy),
        unlocked: false,
    }
}

#[derive(serde::Serialize)]
struct UnlockResult {
    report: LoadReport,
}

#[tauri::command]
async fn list_ports() -> Result<Vec<String>, String> {
    available_ports().map_err(|e| e.to_string())
}

/// Returns the current state of the history. Frontend calls this at startup
/// to decide whether to show the unlock modal.
#[tauri::command]
async fn history_state(state: State<'_, Arc<AppState>>) -> Result<HistoryState, String> {
    info!("history_state called");
    let config = load_config();
    // Rotate before anything else so downstream detect_history_format sees
    // the current (post-rotation) file. Ignore errors — rotation is best-
    // effort, not a hard requirement.
    if let Some(max) = config.history.max_size_mb {
        if let Err(e) = rotate_if_needed(max) {
            warn!(error = %e, "history rotation failed");
        }
    }
    let unlocked = state.history_mode.lock().await.is_some();
    let mut st = detect_state_for(config.history.encrypt);
    if unlocked {
        st.unlocked = true;
    }
    info!(
        encrypt_requested = st.encrypt_requested,
        needs_setup = st.needs_setup,
        existing_encrypted = st.existing_encrypted,
        unlocked = st.unlocked,
        "history_state returning"
    );
    Ok(st)
}

/// Tries to unlock (or set up) the encrypted history with the given
/// passphrase. On success, the mode and writer are stored in app state and
/// ready for `connect_device` to persist live messages.
#[tauri::command]
async fn unlock_history(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    passphrase: String,
) -> Result<UnlockResult, String> {
    let config = load_config();
    if !config.history.encrypt {
        // No encryption requested — nothing to unlock. Fall back to
        // plaintext mode so `connect_device` can proceed.
        let mode = HistoryMode::Plaintext;
        commit_mode(&app, state, mode).await
    } else {
        let path = history_file_path().ok_or("no data dir available")?;
        let mode = match detect_history_format(&path) {
            DetectedFormat::Empty => init_new_v2(&passphrase).map_err(|e| e.to_string())?,
            DetectedFormat::V2 { salt } => {
                unlock_v2(salt, &passphrase).map_err(|e| e.to_string())?
            }
            DetectedFormat::V1Legacy => {
                return Err(
                    "legacy v1 history file — move it aside or re-create the history".into(),
                );
            }
            DetectedFormat::Plaintext => {
                return Err(
                    "existing plaintext history file — disable encrypt or move it aside".into(),
                );
            }
        };
        commit_mode(&app, state, mode).await
    }
}

async fn commit_mode(
    app: &AppHandle,
    state: State<'_, Arc<AppState>>,
    mode: HistoryMode,
) -> Result<UnlockResult, String> {
    // Replay persisted history into the webview.
    let mut restored_msgs: Vec<ChatMessage> = Vec::new();
    let report: LoadReport = load_history(&mode, |m| restored_msgs.push(m));
    info!(
        restored = report.restored,
        errors = report.errors,
        encrypted = mode.is_encrypted(),
        "history loaded"
    );
    for m in &restored_msgs {
        emit_mesh_event(app, &MeshEvent::TextMessage(m.clone()));
    }

    *state.history.lock().await = Some(HistoryWriter::open(mode.clone()));
    *state.history_mode.lock().await = Some(mode);
    Ok(UnlockResult { report })
}

#[tauri::command]
async fn connect_device(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    port: String,
    network: Option<String>,
) -> Result<(), String> {
    info!(%port, network = ?network, "connect_device");

    // If encryption is enabled but not yet unlocked, the frontend must
    // call unlock_history first.
    if state.history_mode.lock().await.is_none() {
        let config = load_config();
        if config.history.encrypt {
            return Err("history still locked — call unlock_history first".into());
        }
        *state.history_mode.lock().await = Some(HistoryMode::Plaintext);
        *state.history.lock().await = Some(HistoryWriter::open(HistoryMode::Plaintext));
    }

    // Run the whole mesh backend on a dedicated OS thread + tokio runtime,
    // *isolated* from Tauri's main runtime. The webview event loop would
    // otherwise starve the serial read task, leaving us with fragmented
    // 1-3 byte reads that never resync on the 0x94 0xC3 framing marker.
    //
    // We use a pair of mpsc channels to bridge the two runtimes:
    //   backend runtime ──events──▶ bridge_events_rx (handled on Tauri rt)
    //   bridge_cmds_tx (Tauri rt) ──▶ backend runtime (forwards to handle.commands)
    let (bridge_events_tx, mut bridge_events_rx) = mpsc::channel::<MeshEvent>(256);
    let (bridge_cmds_tx, mut bridge_cmds_rx) = mpsc::channel::<MeshCommand>(64);
    let (start_result_tx, start_result_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let port_for_thread = port.clone();
    // Explicit UI pick wins over config.toml; if missing, use whatever
    // the user last saved in preferences; if still missing, fall back
    // to the config file; if still missing, default to Meshtastic.
    let network_choice = match network.as_deref() {
        Some("meshtastic") => NetworkChoice::Meshtastic,
        Some("meshcore") => NetworkChoice::Meshcore,
        Some(other) => {
            return Err(format!(
                "unknown network {}: expected \"meshtastic\" or \"meshcore\"",
                other
            ));
        }
        None => match state.aliases.lock().await.preferred_network.as_deref() {
            Some("meshtastic") => NetworkChoice::Meshtastic,
            Some("meshcore") => NetworkChoice::Meshcore,
            _ => load_config().general.network,
        },
    };
    // Persist the pick so next launch defaults to the same backend
    // without the user having to touch `config.toml`.
    let persisted = match network_choice {
        NetworkChoice::Meshtastic => "meshtastic",
        NetworkChoice::Meshcore => "meshcore",
    };
    {
        let mut guard = state.aliases.lock().await;
        if guard.preferred_network.as_deref() != Some(persisted) {
            guard.preferred_network = Some(persisted.to_string());
            let snapshot = guard.clone();
            drop(guard);
            if let Err(e) = mesh_lib::aliases::save(&snapshot) {
                warn!(error = %e, "failed to persist preferred_network");
            }
        }
    }
    info!(network = ?network_choice, "backend selected");
    std::thread::Builder::new()
        .name("mesh-backend".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("mesh-backend-worker")
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    if let Err(send_err) =
                        start_result_tx.send(Err(format!("runtime build: {}", e)))
                    {
                        warn!(error = %send_err, "start_result channel closed before runtime error could be reported");
                    }
                    return;
                }
            };

            rt.block_on(async move {
                let backend: Box<dyn MeshBackend> = match network_choice {
                    NetworkChoice::Meshtastic => {
                        Box::new(MeshtasticBackend::new(port_for_thread))
                    }
                    NetworkChoice::Meshcore => Box::new(MeshcoreBackend::new(port_for_thread)),
                };
                let handle = match backend.start().await {
                    Ok(h) => h,
                    Err(e) => {
                        if let Err(send_err) = start_result_tx.send(Err(e.to_string())) {
                            warn!(error = %send_err, "start_result channel closed before backend error could be reported");
                        }
                        return;
                    }
                };
                if let Err(e) = start_result_tx.send(Ok(())) {
                    debug!(error = %e, "start_result channel closed before ack");
                }
                let mesh_core::BackendHandle {
                    mut events,
                    commands,
                } = handle;

                loop {
                    tokio::select! {
                        maybe_evt = events.recv() => {
                            match maybe_evt {
                                Some(evt) => {
                                    if bridge_events_tx.send(evt).await.is_err() {
                                        break; // Tauri side dropped the receiver
                                    }
                                }
                                None => break,
                            }
                        }
                        maybe_cmd = bridge_cmds_rx.recv() => {
                            let Some(cmd) = maybe_cmd else { break };
                            if commands.send(cmd).await.is_err() {
                                break; // backend dropped its receiver
                            }
                        }
                    }
                }
                info!("mesh-backend thread exiting");
            });
        })
        .map_err(|e| format!("spawn backend thread: {}", e))?;

    // Wait for the backend to finish its initial handshake (success or error).
    match start_result_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err("backend thread died during startup".into()),
    }

    *state.cmd_tx.lock().await = Some(bridge_cmds_tx);

    // Forward every MeshEvent to the webview, and record text messages to
    // disk on the way. This stays on Tauri's runtime since it only does
    // IPC + short Mutex critical sections.
    let state_clone = state.inner().clone();
    tokio::spawn(async move {
        info!("webview forwarder ready");
        while let Some(evt) = bridge_events_rx.recv().await {
            let kind = match &evt {
                MeshEvent::Connected { .. } => "Connected",
                MeshEvent::Disconnected { .. } => "Disconnected",
                MeshEvent::TextMessage(_) => "TextMessage",
                MeshEvent::NodeSeen(_) => "NodeSeen",
                MeshEvent::NodeRemoved { .. } => "NodeRemoved",
                MeshEvent::RepeaterLoginResult { .. } => "RepeaterLoginResult",
                MeshEvent::ChannelInfo(_) => "ChannelInfo",
                MeshEvent::LoraInfo(_) => "LoraInfo",
                MeshEvent::DeviceRoleInfo(_) => "DeviceRoleInfo",
                MeshEvent::ConfigComplete { .. } => "ConfigComplete",
                MeshEvent::SendResult { .. } => "SendResult",
                MeshEvent::SendAck { .. } => "SendAck",
                MeshEvent::Reaction { .. } => "Reaction",
                MeshEvent::Position { .. } => "Position",
                MeshEvent::Telemetry { .. } => "Telemetry",
                MeshEvent::NetworkInfo { .. } => "NetworkInfo",
                MeshEvent::MqttInfo { .. } => "MqttInfo",
                MeshEvent::Error { .. } => "Error",
            };
            info!(kind, "forwarding mesh event to webview");
            match &evt {
                MeshEvent::Connected { my_id, network } => {
                    *state_clone.my_id.lock().await = Some(my_id.clone());
                    *state_clone.current_network.lock().await = Some(*network);
                }
                MeshEvent::Disconnected { .. } => {
                    *state_clone.my_id.lock().await = None;
                }
                MeshEvent::TextMessage(msg) => {
                    if let Some(h) = state_clone.history.lock().await.as_mut() {
                        h.record(msg);
                    }
                }
                _ => {}
            }
            if let Err(e) = app.emit("mesh-event", &evt) {
                warn!(error = %e, "failed to emit mesh-event");
                break;
            }
        }
        info!("webview forwarder exiting");
    });

    Ok(())
}

#[tauri::command]
async fn send_text(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    channel: u32,
    text: String,
    to: Option<String>,
    reply_to_text: Option<String>,
) -> Result<u64, String> {
    let local_id = state.next_local_id.fetch_add(1, Ordering::Relaxed) + 1;

    // When replying, the wire payload gets a `> quote\n` prefix so clients
    // without mesh-chat still see the reference. Our own ChatMessage keeps
    // the clean body + the quote in `reply_to_text` so our UI can render
    // a distinct quote block above the bubble text.
    let wire_text = match reply_to_text.as_deref() {
        Some(quote) if !quote.trim().is_empty() => format!("> {}\n{}", quote, text),
        _ => text.clone(),
    };

    let tx_guard = state.cmd_tx.lock().await;
    let tx = tx_guard.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SendText {
        local_id,
        channel,
        text: wire_text,
        to: to.clone(),
    })
    .await
    .map_err(|e| e.to_string())?;
    drop(tx_guard);

    // No synthetic local-echo here. Both backends now emit their own
    // outgoing-bubble TextMessage on success (Meshtastic via the
    // PacketRouter loopback, Meshcore via an explicit synthesized event
    // in `meshcore_backend::send_text`), so emitting another one here
    // produced two bubbles for every send. The history is recorded by
    // the webview forwarder when the backend's TextMessage comes
    // through, not from this command. The `reply_to_text` parameter
    // is consumed when building `wire_text` above; a quoted preview on
    // our own bubble is reconstructed by the receiving side from the
    // `> quote\n` prefix.
    let _ = reply_to_text;
    let _ = text;
    let _ = app;
    Ok(local_id)
}

/// Converts a named PSK preset into raw bytes.
///
/// Mirrors the TUI editor's options. Randomness is CSPRNG-grade
/// (ChaCha-seeded ThreadRng via `rand::rng()`).
fn psk_from_preset(preset: &str) -> Result<Vec<u8>, String> {
    use rand::RngExt;
    match preset {
        "none" => Ok(Vec::new()),
        "default" | "default1" => Ok(vec![1]),
        "default2" => Ok(vec![2]),
        "default3" => Ok(vec![3]),
        "default4" => Ok(vec![4]),
        "default5" => Ok(vec![5]),
        "default6" => Ok(vec![6]),
        "default7" => Ok(vec![7]),
        "default8" => Ok(vec![8]),
        "default9" => Ok(vec![9]),
        "default10" => Ok(vec![10]),
        "random16" => {
            let mut b = vec![0u8; 16];
            rand::rng().fill(&mut b[..]);
            Ok(b)
        }
        "random32" => {
            let mut b = vec![0u8; 32];
            rand::rng().fill(&mut b[..]);
            Ok(b)
        }
        other => Err(format!("unknown psk preset: {}", other)),
    }
}

#[tauri::command]
async fn upsert_channel(
    state: State<'_, Arc<AppState>>,
    index: u32,
    name: String,
    psk_preset: String,
) -> Result<(), String> {
    if index == 0 {
        return Err("primary channel (index 0) is read-only from the UI".into());
    }
    if index >= 8 {
        return Err("channel index must be in [0, 8)".into());
    }
    if name.trim().is_empty() {
        return Err("channel name cannot be empty".into());
    }
    if name.len() > 11 {
        return Err("channel name too long (max 11 chars)".into());
    }
    let psk = psk_from_preset(&psk_preset)?;
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SetChannel {
        index,
        role: ChannelRole::Secondary,
        name,
        psk,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Parses a user-supplied PSK string: hex (32 or 64 chars → 16 / 32 bytes)
/// or standard base64 (decoding to 16 / 32 bytes). Whitespace stripped.
fn parse_custom_psk(input: &str) -> Result<Vec<u8>, String> {
    let trimmed: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if trimmed.is_empty() {
        return Err("psk is empty".into());
    }
    // Try hex first (unambiguous: only [0-9a-fA-F]).
    if (trimmed.len() == 32 || trimmed.len() == 64)
        && trimmed.chars().all(|c| c.is_ascii_hexdigit())
    {
        let bytes = hex_decode(&trimmed).map_err(|e| format!("hex decode: {}", e))?;
        if bytes.len() == 16 || bytes.len() == 32 {
            return Ok(bytes);
        }
    }
    // Fall back to base64. Accept standard or URL-safe.
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD
        .decode(&trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&trimmed))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&trimmed))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&trimmed));
    match b64 {
        Ok(bytes) if bytes.len() == 16 || bytes.len() == 32 => Ok(bytes),
        Ok(bytes) => Err(format!(
            "decoded PSK has {} bytes, expected 16 or 32",
            bytes.len()
        )),
        Err(_) => Err("not valid hex or base64".into()),
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd length".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push(hi * 16 + lo);
    }
    Ok(out)
}

fn nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(10 + b - b'a'),
        b'A'..=b'F' => Ok(10 + b - b'A'),
        other => Err(format!("invalid hex char: 0x{:02x}", other)),
    }
}

#[tauri::command]
async fn upsert_channel_custom(
    state: State<'_, Arc<AppState>>,
    index: u32,
    name: String,
    psk: String,
    psk_confirm: String,
) -> Result<(), String> {
    if index == 0 {
        return Err("primary channel (index 0) is read-only from the UI".into());
    }
    if index >= 8 {
        return Err("channel index must be in [0, 8)".into());
    }
    if name.trim().is_empty() {
        return Err("channel name cannot be empty".into());
    }
    if name.len() > 11 {
        return Err("channel name too long (max 11 chars)".into());
    }
    if psk != psk_confirm {
        return Err("PSK confirmation does not match".into());
    }
    let psk_bytes = parse_custom_psk(&psk)?;
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SetChannel {
        index,
        role: ChannelRole::Secondary,
        name,
        psk: psk_bytes,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(serde::Serialize)]
struct ChannelShare {
    url: String,
    qr_svg: String,
}

#[tauri::command]
async fn channel_share_fields(
    name: String,
    psk: Vec<u8>,
    uplink_enabled: bool,
    downlink_enabled: bool,
) -> Result<ChannelShare, String> {
    use qrcode::QrCode;

    let info = mesh_core::ChannelInfo {
        network: mesh_core::Network::Meshtastic,
        index: 0,
        role: mesh_core::ChannelRole::Secondary,
        name,
        psk,
        uplink_enabled,
        downlink_enabled,
    };
    let url = meshtastic_backend::encode_channel_share_url(&info).map_err(|e| e.to_string())?;
    let qr = QrCode::new(url.as_bytes()).map_err(|e| format!("qr encode: {}", e))?;
    let svg = qr
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .dark_color(qrcode::render::svg::Color("#eef2fa"))
        .light_color(qrcode::render::svg::Color("#0e1320"))
        .build();
    Ok(ChannelShare { url, qr_svg: svg })
}

#[tauri::command]
async fn delete_channel(state: State<'_, Arc<AppState>>, index: u32) -> Result<(), String> {
    if index == 0 {
        return Err("primary channel (index 0) is read-only".into());
    }
    if index >= 8 {
        return Err("channel index must be in [0, 8)".into());
    }
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SetChannel {
        index,
        role: ChannelRole::Disabled,
        name: String::new(),
        psk: Vec::new(),
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn update_user(
    state: State<'_, Arc<AppState>>,
    long_name: String,
    short_name: String,
) -> Result<(), String> {
    if long_name.trim().is_empty() || short_name.trim().is_empty() {
        return Err("long and short names cannot be empty".into());
    }
    if long_name.len() > 39 {
        return Err("long name too long (max 39 chars)".into());
    }
    if short_name.len() > 4 {
        return Err("short name too long (max 4 chars)".into());
    }
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::UpdateUser {
        long_name,
        short_name,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Write a new LoRa radio config. The backend validates the enum names and
/// guardrails; this command does a shallow frontend-side sanity check and
/// dispatches. The user must also have gone through a confirm dialog in
/// the UI — changing region or preset reboots the radio.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn set_lora_config(
    state: State<'_, Arc<AppState>>,
    region: String,
    modem_preset: String,
    use_preset: bool,
    hop_limit: u32,
    tx_enabled: bool,
    tx_power: i32,
) -> Result<(), String> {
    if region.trim().is_empty() {
        return Err("region cannot be empty".into());
    }
    if modem_preset.trim().is_empty() {
        return Err("modem preset cannot be empty".into());
    }
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SetLoraConfig {
        region,
        modem_preset,
        use_preset,
        hop_limit,
        tx_enabled,
        tx_power,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Write a new device role. Caller must have shown a confirm dialog —
/// changing role affects battery life and mesh behaviour.
#[tauri::command]
async fn set_device_role(
    state: State<'_, Arc<AppState>>,
    role: String,
) -> Result<(), String> {
    if role.trim().is_empty() {
        return Err("role cannot be empty".into());
    }
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SetDeviceRole { role })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Which backend the Tauri app is currently talking to. Returns `"none"`
/// before any `Connected` event has arrived; `"meshtastic"` / `"meshcore"`
/// once the backend has finished its handshake. The webview uses this to
/// gate protocol-specific UI (e.g. disable emoji reactions on Meshcore).
#[tauri::command]
async fn get_network(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let net = state.current_network.lock().await;
    Ok(match *net {
        Some(Network::Meshtastic) => "meshtastic",
        Some(Network::Meshcore) => "meshcore",
        None => "none",
    }
    .to_string())
}

/// Send an emoji reaction to a prior message. Requires Meshtastic; the
/// Meshcore backend will emit a `SendResult(ok=false)` with an explicit
/// error message but this command still returns `Ok` — the frontend's
/// `SendResult` handler is what surfaces the failure to the user.
#[tauri::command]
async fn send_reaction(
    state: State<'_, Arc<AppState>>,
    channel: u32,
    to: Option<String>,
    reply_to_packet_id: u32,
    emoji: String,
) -> Result<u64, String> {
    if emoji.trim().is_empty() {
        return Err("emoji cannot be empty".into());
    }
    let local_id = state.next_local_id.fetch_add(1, Ordering::Relaxed) + 1;
    let tx_guard = state.cmd_tx.lock().await;
    let tx = tx_guard.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SendReaction {
        local_id,
        channel,
        to,
        reply_to_packet_id,
        emoji,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(local_id)
}

/// Ask the backend to re-emit its known-nodes list. Useful when a
/// remote node got renamed and the UI still shows the old cached
/// name. Meshcore re-queries its contact table; Meshtastic surfaces
/// a "not supported" error event.
#[tauri::command]
async fn refresh_nodes(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::RefreshNodes)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Rebroadcast our own full-identity advertisement (Meshcore only).
/// Use this when a remote can't receive our DMs — it usually means
/// the remote doesn't have our pubkey cached, and a fresh advert lets
/// it add us to its contact table.
#[tauri::command]
async fn send_advert(
    flood: bool,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SendAdvert { flood })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Remove a node from the radio's contact cache (Meshcore only).
/// UI should confirm with the user first since there's no undo on the
/// firmware side — the node will reappear only if it advertises again.
#[tauri::command]
async fn forget_node(
    id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::ForgetNode { id })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Log into a Meshcore repeater or room server with its admin password.
/// The session lasts ~10–15 min on the firmware before the user has to
/// re-authenticate. Reply lands as a `RepeaterLoginResult` event the UI
/// listens to.
#[tauri::command]
async fn repeater_login(
    peer: String,
    password: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::RepeaterLogin { peer, password })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Tear down an active admin session with a Meshcore repeater.
#[tauri::command]
async fn repeater_logout(
    peer: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::RepeaterLogout { peer })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Write WiFi credentials to the radio. Empty PSK = open network.
/// Caller must have shown a confirm dialog since changing WiFi
/// settings reboots the firmware.
#[tauri::command]
async fn set_network_config(
    state: State<'_, Arc<AppState>>,
    wifi_enabled: bool,
    wifi_ssid: String,
    wifi_psk: String,
) -> Result<(), String> {
    if wifi_ssid.len() > 32 {
        return Err("WiFi SSID too long (max 32 chars)".into());
    }
    if !wifi_psk.is_empty() && wifi_psk.len() < 8 {
        return Err("WiFi PSK must be empty (open) or ≥ 8 chars (WPA2)".into());
    }
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SetNetworkConfig {
        wifi_enabled,
        wifi_ssid,
        wifi_psk,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Write MQTT module config. For the public Meshtastic broker, the
/// firmware accepts an empty `address` (falls back to its built-in
/// default `mqtt.meshtastic.org`). Anything the user types wins.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn set_mqtt_config(
    state: State<'_, Arc<AppState>>,
    enabled: bool,
    address: String,
    username: String,
    password: String,
    encryption_enabled: bool,
    tls_enabled: bool,
    map_reporting_enabled: bool,
    root: String,
) -> Result<(), String> {
    let tx = state.cmd_tx.lock().await;
    let tx = tx.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SetMqttConfig {
        enabled,
        address,
        username,
        password,
        encryption_enabled,
        tls_enabled,
        map_reporting_enabled,
        root,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Wipe all persisted chat history. Requires a prior UI confirm dialog —
/// the destructive action is irreversible. Closes and drops the current
/// `HistoryWriter` before unlinking the file (Windows refuses to delete
/// an open file), then re-opens a fresh writer in the same mode so new
/// messages resume being persisted immediately.
#[tauri::command]
async fn clear_history(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    // Drop the existing writer first so the file handle is released.
    let mode = state
        .history_mode
        .lock()
        .await
        .clone()
        .ok_or("history mode not initialized")?;
    *state.history.lock().await = None;

    delete_history_files().map_err(|e| e.to_string())?;

    // Re-create a fresh writer so subsequent messages keep being recorded.
    *state.history.lock().await = Some(HistoryWriter::open(mode));
    info!("history cleared on user request");
    Ok(())
}

/// Share the user's current position. Broadcasts on channel 0 via
/// PortNum::PositionApp (Meshtastic) or persists to the radio's advert
/// (Meshcore). Coordinates must be valid WGS84.
#[tauri::command]
async fn send_position(
    state: State<'_, Arc<AppState>>,
    latitude: f64,
    longitude: f64,
) -> Result<u64, String> {
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return Err(format!(
            "invalid position: lat={} lon={}",
            latitude, longitude
        ));
    }
    let local_id = state.next_local_id.fetch_add(1, Ordering::Relaxed) + 1;
    let tx_guard = state.cmd_tx.lock().await;
    let tx = tx_guard.as_ref().ok_or("not connected")?;
    tx.send(MeshCommand::SendPosition {
        local_id,
        latitude,
        longitude,
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(local_id)
}

/// Snapshot of user overrides for the webview: alias map + favorite
/// set + preferred backend (so the UI's Connect card pre-selects the
/// last backend the user picked without reading `config.toml`).
#[derive(serde::Serialize)]
struct AliasSnapshot {
    aliases: std::collections::HashMap<String, String>,
    favorites: Vec<String>,
    preferred_network: Option<String>,
}

#[tauri::command]
async fn get_aliases(state: State<'_, Arc<AppState>>) -> Result<AliasSnapshot, String> {
    let a = state.aliases.lock().await;
    Ok(AliasSnapshot {
        aliases: a.aliases.clone(),
        favorites: a.favorites.clone(),
        preferred_network: a.preferred_network.clone(),
    })
}

#[tauri::command]
async fn set_alias(
    state: State<'_, Arc<AppState>>,
    node_id: String,
    alias: Option<String>,
) -> Result<(), String> {
    if node_id.trim().is_empty() {
        return Err("node_id cannot be empty".into());
    }
    let mut guard = state.aliases.lock().await;
    guard.set(node_id, alias);
    let snapshot = guard.clone();
    drop(guard);
    // Persist outside the lock — the JSON write shouldn't block other
    // readers during a save.
    mesh_lib::aliases::save(&snapshot).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn set_favorite(
    state: State<'_, Arc<AppState>>,
    node_id: String,
    favorite: bool,
) -> Result<(), String> {
    if node_id.trim().is_empty() {
        return Err("node_id cannot be empty".into());
    }
    let mut guard = state.aliases.lock().await;
    guard.set_favorite(node_id, favorite);
    let snapshot = guard.clone();
    drop(guard);
    mesh_lib::aliases::save(&snapshot).map_err(|e| e.to_string())?;
    Ok(())
}

/// User-initiated disconnect — signals the running backend to shut down,
/// then wipes the Tauri-side bridge state so a subsequent `connect_device`
/// starts cleanly. Also used as the window-close handler. Idempotent.
#[tauri::command]
async fn shutdown(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    // Take the sender out so subsequent commands that need `cmd_tx` fail
    // fast ("not connected") instead of racing with an old pending write.
    let taken = state.cmd_tx.lock().await.take();
    if let Some(tx) = taken {
        if let Err(e) = tx.send(MeshCommand::Shutdown).await {
            // Channel already closed = backend already exited. Treat as
            // idempotent success rather than an error surfaced to the UI.
            debug!(error = %e, "shutdown: backend command channel already closed");
        }
    }
    // Clear the cached identity + network so the UI reflects the real
    // disconnected state (no stale `my_id` chip, etc.).
    *state.my_id.lock().await = None;
    *state.current_network.lock().await = None;
    info!("backend disconnect complete");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| mesh_lib::logging::DEFAULT_FILTER.into()),
        )
        .init();

    // Load aliases/favorites from disk before Tauri state is created
    // so the webview's first `get_aliases` call returns real data.
    let app_state = Arc::new(AppState::default());
    let loaded = mesh_lib::aliases::load();
    *app_state.aliases.blocking_lock() = loaded;

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            list_ports,
            history_state,
            unlock_history,
            connect_device,
            send_text,
            upsert_channel,
            upsert_channel_custom,
            channel_share_fields,
            delete_channel,
            update_user,
            set_lora_config,
            set_device_role,
            get_aliases,
            set_alias,
            set_favorite,
            get_network,
            send_reaction,
            send_position,
            set_network_config,
            set_mqtt_config,
            refresh_nodes,
            send_advert,
            forget_node,
            repeater_login,
            repeater_logout,
            clear_history,
            shutdown,
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}
