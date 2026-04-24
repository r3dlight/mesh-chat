//! Meshcore backend: talks to Meshcore companion-radio firmware over serial.
//!
//! Thin adapter around the upstream `meshcore-rs` crate. The companion-radio
//! wire protocol between host and radio is plaintext binary (command
//! opcodes + little-endian lengths); all mesh-level cryptography
//! (Curve25519 identity, per-channel 16-byte AES secrets, ACK signatures)
//! happens inside the radio firmware, so this adapter never touches key
//! material directly.
//!
//! Mapping of our generic types to Meshcore concepts:
//!
//! - `my_id` / peer id → 12 lowercase-hex chars of the 6-byte public-key
//!   prefix (same encoding as `ContactMessage::sender_prefix_hex`).
//! - `MeshCommand::SendText { to: Some(peer) }` →
//!   `commands().send_msg(contact, text)` after a contact-cache lookup by
//!   prefix.
//! - `MeshCommand::SendText { to: None, channel }` →
//!   `commands().send_channel_msg(channel as u8, text)`.
//! - `MeshCommand::SetChannel` requires a 16-byte PSK (Meshcore's
//!   `CHANNEL_SECRET_LEN`); 0/1/32-byte PSKs from the Meshtastic flow are
//!   rejected with an explicit error event.
//! - `MeshCommand::SetLoraConfig` / `SetDeviceRole` are Meshtastic-specific
//!   and reported as unsupported.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use mesh_core::{
    BackendHandle, ChannelInfo, ChannelRole, ChatMessage, MeshBackend, MeshCommand, MeshEvent,
    Network, NodeInfo,
};
use meshcore_rs::{events::EventPayload, EventType, MeshCore, MeshCoreEvent};
use tokio::sync::mpsc;
use tracing::{debug, info, instrument, warn};

/// Baud rate used by the Meshcore companion-radio firmware on USB CDC.
const DEFAULT_BAUD: u32 = 115_200;

/// Bytes of public key used as a public-facing node id.
const PREFIX_LEN: usize = 6;

/// Backend talking to a Meshcore companion radio on a serial port.
pub struct MeshcoreBackend {
    serial_port: String,
}

impl MeshcoreBackend {
    pub fn new(serial_port: impl Into<String>) -> Self {
        Self {
            serial_port: serial_port.into(),
        }
    }
}

#[async_trait]
impl MeshBackend for MeshcoreBackend {
    fn network(&self) -> Network {
        Network::Meshcore
    }

    #[instrument(skip(self), fields(port = %self.serial_port))]
    async fn start(&self) -> Result<BackendHandle> {
        let (event_tx, event_rx) = mpsc::channel::<MeshEvent>(256);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<MeshCommand>(64);

        let mc = MeshCore::serial(&self.serial_port, DEFAULT_BAUD).await?;
        info!(port = %self.serial_port, "meshcore serial opened");

        // APP_START is mandatory — the radio won't emit any events until
        // the host has identified itself. The response carries the node's
        // public key and advertised name.
        let self_info = mc.commands().lock().await.send_appstart().await?;
        let my_id = hex_prefix(&self_info.public_key);
        info!(my_id = %my_id, name = %self_info.name, "meshcore appstart done");

        emit(
            &event_tx,
            MeshEvent::Connected {
                network: Network::Meshcore,
                my_id: my_id.clone(),
            },
        )
        .await;
        emit(
            &event_tx,
            MeshEvent::NodeSeen(NodeInfo {
                network: Network::Meshcore,
                id: my_id.clone(),
                long_name: self_info.name.clone(),
                short_name: short_from_long(&self_info.name),
                battery_level: None,
                voltage: None,
                snr: None,
                last_heard: Some(chrono::Utc::now().timestamp()),
                hops_away: None,
            }),
        )
        .await;

        // Populate the sidebar from the firmware's contact cache. Failures
        // here are non-fatal: advertisements will rebuild the list as they
        // arrive from the mesh.
        match mc.commands().lock().await.get_contacts(0).await {
            Ok(contacts) => {
                debug!(count = contacts.len(), "meshcore contacts loaded");
                for c in contacts {
                    emit(&event_tx, contact_to_node(&c)).await;
                }
            }
            Err(e) => warn!(error = %e, "meshcore get_contacts failed; sidebar will fill from adverts"),
        }

        // Re-hydrate the channel list from whatever the firmware has in
        // flash. Meshtastic pushes all ChannelInfo packets automatically
        // during `configure(id)` — Meshcore does not, so we poll.
        // `max_channels` only exists on fw v3+; fall back to 8 (the
        // Meshtastic-default range) on older firmware.
        let max_channels = match mc.commands().lock().await.send_device_query().await {
            Ok(info) => {
                debug!(fw = info.fw_version_code, max = ?info.max_contacts, "meshcore device info");
                info.max_channels.unwrap_or(8)
            }
            Err(e) => {
                warn!(error = %e, "meshcore send_device_query failed; assuming 8 channels");
                8
            }
        };
        for idx in 0..max_channels {
            match mc.commands().lock().await.get_channel(idx).await {
                Ok(info) => {
                    // A freshly-initialized slot often has an empty name
                    // and an all-zero secret — skip those so the UI
                    // doesn't show ghost channels. A real, written
                    // channel has either a name or a non-zero secret.
                    let has_name = !info.name.trim().is_empty();
                    let has_secret = info.secret.iter().any(|b| *b != 0);
                    if !has_name && !has_secret {
                        continue;
                    }
                    emit(
                        &event_tx,
                        MeshEvent::ChannelInfo(ChannelInfo {
                            network: Network::Meshcore,
                            index: u32::from(idx),
                            role: ChannelRole::Secondary,
                            name: info.name,
                            psk: info.secret.to_vec(),
                            uplink_enabled: true,
                            downlink_enabled: true,
                        }),
                    )
                    .await;
                }
                Err(e) => {
                    // Index out of range or other firmware-side error —
                    // stop iterating, anything past here is empty anyway.
                    debug!(index = idx, error = %e, "meshcore get_channel stopped");
                    break;
                }
            }
        }

        // Hook the firmware's "messages waiting" push notification so missed
        // messages are pulled automatically.
        mc.start_auto_message_fetching().await;

        // Best-effort battery snapshot for the stats panel. The Meshcore
        // companion protocol has no periodic TelemetryApp equivalent, so
        // we poll `get_bat` once at startup and then every minute in the
        // background. Failures are non-fatal — the stats panel just
        // stays empty for our own node in that case.
        if let Ok(batt) = mc.commands().lock().await.get_bat().await {
            emit(
                &event_tx,
                MeshEvent::Telemetry {
                    network: Network::Meshcore,
                    from: my_id.clone(),
                    battery_level: Some(u32::from(batt.percentage())),
                    voltage: Some(batt.voltage()),
                    channel_utilization: None,
                    air_util_tx: None,
                    uptime_seconds: None,
                    timestamp: chrono::Utc::now().timestamp(),
                },
            )
            .await;
        }

        emit(
            &event_tx,
            MeshEvent::ConfigComplete {
                network: Network::Meshcore,
            },
        )
        .await;

        let mc = Arc::new(mc);

        // Periodic battery poll so the stats panel stays current without
        // relying on push notifications the firmware doesn't emit.
        let mc_batt = mc.clone();
        let event_tx_batt = event_tx.clone();
        let my_id_batt = my_id.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            // Skip the first fire — we've already emitted one snapshot
            // above as part of the startup sequence.
            tick.tick().await;
            loop {
                tick.tick().await;
                let res = mc_batt.commands().lock().await.get_bat().await;
                if let Ok(batt) = res {
                    emit(
                        &event_tx_batt,
                        MeshEvent::Telemetry {
                            network: Network::Meshcore,
                            from: my_id_batt.clone(),
                            battery_level: Some(u32::from(batt.percentage())),
                            voltage: Some(batt.voltage()),
                            channel_utilization: None,
                            air_util_tx: None,
                            uptime_seconds: None,
                            timestamp: chrono::Utc::now().timestamp(),
                        },
                    )
                    .await;
                }
                if event_tx_batt.is_closed() {
                    break;
                }
            }
        });


        // Event pump: meshcore-rs broadcast stream → our mpsc.
        let mc_stream = mc.clone();
        let event_tx_stream = event_tx.clone();
        let my_id_stream = my_id.clone();
        tokio::spawn(async move {
            let mut stream = mc_stream.event_stream();
            while let Some(ev) = stream.next().await {
                for mapped in map_event(ev, &my_id_stream) {
                    emit(&event_tx_stream, mapped).await;
                }
            }
            debug!("meshcore event stream closed");
        });

        // Command loop: translate our generic commands to meshcore-rs calls.
        let mc_cmd = mc.clone();
        let event_tx_cmd = event_tx.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if matches!(cmd, MeshCommand::Shutdown) {
                    info!("meshcore shutdown requested");
                    if let Err(e) = mc_cmd.disconnect().await {
                        warn!(error = %e, "meshcore disconnect failed");
                    }
                    break;
                }
                handle_cmd(&mc_cmd, cmd, &event_tx_cmd).await;
            }
            debug!("meshcore command loop exiting");
        });

        Ok(BackendHandle {
            events: event_rx,
            commands: cmd_tx,
        })
    }
}

/// Best-effort send on the event channel. If the receiver has been dropped
/// the UI is shutting down; we log at debug and move on.
async fn emit(tx: &mpsc::Sender<MeshEvent>, evt: MeshEvent) {
    if let Err(err) = tx.send(evt).await {
        debug!(error = %err, "mesh event dropped (receiver closed)");
    }
}

async fn send_err(tx: &mpsc::Sender<MeshEvent>, message: String) {
    emit(
        tx,
        MeshEvent::Error {
            network: Network::Meshcore,
            message,
        },
    )
    .await;
}

/// Send a SendResult(ok=false) + Error pair. Used when a SendText fails
/// before reaching the radio so the UI can stamp the bubble `✗`.
async fn send_failure(tx: &mpsc::Sender<MeshEvent>, local_id: u64, error: String) {
    emit(
        tx,
        MeshEvent::SendResult {
            network: Network::Meshcore,
            local_id,
            ok: false,
            error: Some(error.clone()),
            packet_id: None,
        },
    )
    .await;
    send_err(tx, error).await;
}

/// Render a 32-byte public key as the 6-byte `PREFIX_LEN` hex identifier used
/// throughout our API and matching `ContactMessage::sender_prefix_hex`.
fn hex_prefix(pubkey: &[u8]) -> String {
    let take = pubkey.len().min(PREFIX_LEN);
    bytes_hex(&pubkey[..take])
}

/// Meshcore only advertises a single name; we synthesise a 4-char short
/// identifier to match the Meshtastic `short_name` convention used by the UI.
///
/// - Multi-word → up to 4 initials (e.g. "Alpha Bravo Charlie" → "ABC").
/// - Single-word → first 4 characters (e.g. "redlight" → "redl", "hi" → "hi").
/// - Empty → empty.
fn short_from_long(long: &str) -> String {
    let words: Vec<&str> = long.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    if words.len() == 1 {
        return words[0].chars().take(4).collect();
    }
    words
        .iter()
        .filter_map(|w| w.chars().next())
        .take(4)
        .collect()
}

fn contact_to_node(c: &meshcore_rs::events::Contact) -> MeshEvent {
    MeshEvent::NodeSeen(NodeInfo {
        network: Network::Meshcore,
        id: c.prefix_hex(),
        long_name: c.adv_name.clone(),
        short_name: short_from_long(&c.adv_name),
        battery_level: None,
        voltage: None,
        snr: None,
        last_heard: if c.last_advert == 0 {
            None
        } else {
            Some(c.last_advert as i64)
        },
        hops_away: if c.path_len < 0 {
            None
        } else {
            Some(c.path_len as u32)
        },
    })
}

/// Translate one meshcore-rs event to zero, one, or multiple generic
/// `MeshEvent`s. A single upstream event can fan out — e.g. an
/// Advertisement carries both node identity AND position, both of
/// which the UI layer consumes separately.
fn map_event(ev: MeshCoreEvent, my_id: &str) -> Vec<MeshEvent> {
    use EventPayload as EP;
    match (ev.event_type, ev.payload) {
        (EventType::Disconnected, _) => vec![MeshEvent::Disconnected {
            network: Network::Meshcore,
        }],
        (EventType::ContactMsgRecv, EP::ContactMessage(msg)) => {
            vec![MeshEvent::TextMessage(ChatMessage {
                timestamp: chrono::Utc::now().timestamp(),
                network: Network::Meshcore,
                channel: 0,
                from: msg.sender_prefix_hex(),
                to: my_id.to_string(),
                text: msg.text,
                local_id: None,
                status: None,
                rx_snr: msg.snr,
                rx_rssi: None,
                reply_to_text: None,
                packet_id: None,
                reactions: std::collections::HashMap::new(),
            })]
        }
        (EventType::ChannelMsgRecv, EP::ChannelMessage(msg)) => {
            // Meshcore's companion protocol does not expose the sender's
            // pubkey prefix on channel messages — only the channel index.
            // We render it with a synthetic `chan{n}` sender so the bubble
            // has something to label; once the protocol v2 roadmap lands
            // sender attribution we'll switch to the real prefix.
            vec![MeshEvent::TextMessage(ChatMessage {
                timestamp: chrono::Utc::now().timestamp(),
                network: Network::Meshcore,
                channel: u32::from(msg.channel_idx),
                from: format!("chan{}", msg.channel_idx),
                to: "^all".into(),
                text: msg.text,
                local_id: None,
                status: None,
                rx_snr: msg.snr,
                rx_rssi: None,
                reply_to_text: None,
                packet_id: None,
                reactions: std::collections::HashMap::new(),
            })]
        }
        (EventType::NewContact, EP::Contact(c)) => {
            let mut out = vec![contact_to_node(&c)];
            if let Some(pos) = position_from_contact(&c) {
                out.push(pos);
            }
            out
        }
        // NOTE: `EventType::Contacts` carries `EP::Contacts(Vec<Contact>)`
        // (a full snapshot). We drop it — the initial `get_contacts()` call
        // at startup handles full hydration, and `NewContact` events cover
        // deltas afterwards.
        (EventType::Advertisement, EP::Advertisement(a)) => {
            let mut out = vec![];
            if let Some(pos) = position_from_advertisement(&a) {
                out.push(pos);
            }
            out.push(MeshEvent::NodeSeen(NodeInfo {
                network: Network::Meshcore,
                id: bytes_hex(&a.prefix),
                long_name: a.name,
                short_name: String::new(),
                battery_level: None,
                voltage: None,
                snr: None,
                last_heard: Some(chrono::Utc::now().timestamp()),
                hops_away: None,
            }));
            out
        }
        _ => vec![],
    }
}

/// Meshcore carries position inline with contact/advertisement data
/// (`adv_lat` / `adv_lon`, int × 1e-6 degrees). We surface it as a
/// separate `Position` event whenever the value is non-zero — the UI
/// can then pin it to the contact's most recent bubble exactly like on
/// Meshtastic.
fn position_from_contact(c: &meshcore_rs::events::Contact) -> Option<MeshEvent> {
    if c.adv_lat == 0 && c.adv_lon == 0 {
        return None;
    }
    Some(MeshEvent::Position {
        network: Network::Meshcore,
        from: c.prefix_hex(),
        latitude: f64::from(c.adv_lat) / 1e6,
        longitude: f64::from(c.adv_lon) / 1e6,
        timestamp: chrono::Utc::now().timestamp(),
    })
}

fn position_from_advertisement(a: &meshcore_rs::events::AdvertisementData) -> Option<MeshEvent> {
    if a.lat == 0 && a.lon == 0 {
        return None;
    }
    Some(MeshEvent::Position {
        network: Network::Meshcore,
        from: bytes_hex(&a.prefix),
        latitude: f64::from(a.lat) / 1e6,
        longitude: f64::from(a.lon) / 1e6,
        timestamp: chrono::Utc::now().timestamp(),
    })
}

fn bytes_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    // Lowercase hex, one byte at a time. `format!` on `u8` is infallible so
    // there is no Result to juggle — cleaner than `write!` + error swallow.
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

async fn handle_cmd(mc: &Arc<MeshCore>, cmd: MeshCommand, event_tx: &mpsc::Sender<MeshEvent>) {
    match cmd {
        MeshCommand::SendText {
            local_id,
            channel,
            text,
            to,
        } => send_text(mc, local_id, channel, text, to, event_tx).await,
        MeshCommand::UpdateUser { long_name, .. } => {
            // Meshcore stores a single advertised name; short_name is a
            // Meshtastic-only concept and is dropped here.
            if let Err(e) = mc.commands().lock().await.set_name(&long_name).await {
                warn!(error = %e, "meshcore set_name failed");
                send_err(event_tx, format!("meshcore set_name: {}", e)).await;
            } else {
                info!(%long_name, "meshcore name updated");
            }
        }
        MeshCommand::SetChannel {
            index,
            role: _,
            name,
            psk,
        } => set_channel(mc, index, name, psk, event_tx).await,
        MeshCommand::SetLoraConfig { .. } => {
            send_err(
                event_tx,
                "Meshcore backend does not expose LoRa region/preset as Meshtastic does".into(),
            )
            .await;
        }
        MeshCommand::SetDeviceRole { .. } => {
            send_err(
                event_tx,
                "Meshcore backend does not expose a device-role switch on the companion protocol"
                    .into(),
            )
            .await;
        }
        MeshCommand::SetNetworkConfig { .. } => {
            send_err(
                event_tx,
                "WiFi / Ethernet config requires Meshtastic — Meshcore companion protocol has no equivalent write path".into(),
            )
            .await;
        }
        MeshCommand::SetMqttConfig { .. } => {
            send_err(
                event_tx,
                "MQTT config requires Meshtastic — Meshcore does not ship an MQTT module".into(),
            )
            .await;
        }
        MeshCommand::SendReaction { local_id, .. } => {
            // Meshcore's companion protocol has no emoji-reaction primitive.
            // We refuse rather than fake it as a plain text message (that
            // would add noise to the thread for negative value — see the
            // design note in WORKLOG).
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshcore,
                    local_id,
                    ok: false,
                    error: Some(
                        "reactions require Meshtastic (no native reaction primitive in Meshcore)"
                            .into(),
                    ),
                    packet_id: None,
                },
            )
            .await;
        }
        MeshCommand::SendPosition {
            local_id,
            latitude,
            longitude,
        } => {
            // Meshcore's companion API takes degrees as f64. The new
            // coordinates are baked into subsequent advertisements the
            // radio broadcasts — there's no separate "send position now"
            // primitive, so this is effectively an identity update.
            if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
                emit(
                    event_tx,
                    MeshEvent::SendResult {
                        network: Network::Meshcore,
                        local_id,
                        ok: false,
                        error: Some(format!(
                            "position out of range: lat={} lon={}",
                            latitude, longitude
                        )),
                        packet_id: None,
                    },
                )
                .await;
                return;
            }
            match mc.commands().lock().await.set_coords(latitude, longitude).await {
                Ok(_) => {
                    info!(latitude, longitude, "meshcore coords updated");
                    emit(
                        event_tx,
                        MeshEvent::SendResult {
                            network: Network::Meshcore,
                            local_id,
                            ok: true,
                            error: None,
                            packet_id: None,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    warn!(error = %e, "meshcore set_coords failed");
                    emit(
                        event_tx,
                        MeshEvent::SendResult {
                            network: Network::Meshcore,
                            local_id,
                            ok: false,
                            error: Some(format!("meshcore set_coords: {}", e)),
                            packet_id: None,
                        },
                    )
                    .await;
                }
            }
        }
        MeshCommand::Shutdown => {
            // Handled in the outer loop — the match arm here is unreachable
            // unless the outer dispatcher changes. Keep it explicit so
            // exhaustiveness stays enforced by the compiler.
            debug!("shutdown reached handle_cmd; ignoring (outer loop handles it)");
        }
    }
}

#[instrument(skip(mc, event_tx, text), fields(channel, dm = to.is_some(), bytes = text.len()))]
async fn send_text(
    mc: &Arc<MeshCore>,
    local_id: u64,
    channel: u32,
    text: String,
    to: Option<String>,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    if let Some(peer) = to {
        // DM: look up a known contact by 6-byte prefix.
        let prefix_bytes = match parse_hex_prefix(&peer) {
            Some(b) => b,
            None => {
                send_failure(
                    event_tx,
                    local_id,
                    format!("invalid peer id {}: expect 12 hex chars", peer),
                )
                .await;
                return;
            }
        };
        let contact = mc.get_contact_by_prefix(&prefix_bytes).await;
        let Some(contact) = contact else {
            send_failure(
                event_tx,
                local_id,
                format!("no known meshcore contact with prefix {}", peer),
            )
            .await;
            return;
        };
        let name = contact.adv_name.clone();
        match mc.commands().lock().await.send_msg(contact, &text, None).await {
            Ok(info) => {
                info!(
                    local_id,
                    peer = %name,
                    expected_ack = ?info.expected_ack,
                    "meshcore DM accepted by radio"
                );
                emit(
                    event_tx,
                    MeshEvent::SendResult {
                        network: Network::Meshcore,
                        local_id,
                        ok: true,
                        error: None,
                        packet_id: None,
                    },
                )
                .await;
            }
            Err(e) => {
                warn!(local_id, error = %e, "meshcore send_msg failed");
                send_failure(event_tx, local_id, format!("meshcore send: {}", e)).await;
            }
        }
        return;
    }

    // Channel broadcast. Meshcore's companion protocol takes a u8 channel
    // index; reject anything that can't fit cleanly.
    let chan_idx = match u8::try_from(channel) {
        Ok(v) => v,
        Err(_) => {
            send_failure(
                event_tx,
                local_id,
                format!("channel index {} out of range for Meshcore (u8)", channel),
            )
            .await;
            return;
        }
    };
    match mc
        .commands()
        .lock()
        .await
        .send_channel_msg(chan_idx, &text, None)
        .await
    {
        Ok(()) => {
            info!(local_id, channel = chan_idx, "meshcore channel msg accepted");
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshcore,
                    local_id,
                    ok: true,
                    error: None,
                    packet_id: None,
                },
            )
            .await;
        }
        Err(e) => {
            warn!(local_id, channel = chan_idx, error = %e, "meshcore send_channel_msg failed");
            send_failure(event_tx, local_id, format!("meshcore channel send: {}", e)).await;
        }
    }
}

#[instrument(skip(mc, event_tx, name, psk), fields(index, name_len = name.len(), psk_len = psk.len()))]
async fn set_channel(
    mc: &Arc<MeshCore>,
    index: u32,
    name: String,
    psk: Vec<u8>,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    let chan_idx = match u8::try_from(index) {
        Ok(v) => v,
        Err(_) => {
            send_err(
                event_tx,
                format!("channel index {} out of range for Meshcore (u8)", index),
            )
            .await;
            return;
        }
    };
    // Meshcore uses a fixed 16-byte channel secret; the Meshtastic 0/1/32-byte
    // shortcuts are not representable. Refuse them explicitly rather than
    // silently padding / truncating the key.
    let secret: [u8; meshcore_rs::CHANNEL_SECRET_LEN] = match psk.as_slice().try_into() {
        Ok(a) => a,
        Err(_) => {
            send_err(
                event_tx,
                format!(
                    "meshcore channel secret must be exactly {} bytes, got {}",
                    meshcore_rs::CHANNEL_SECRET_LEN,
                    psk.len()
                ),
            )
            .await;
            return;
        }
    };
    match mc
        .commands()
        .lock()
        .await
        .set_channel(chan_idx, &name, &secret)
        .await
    {
        Ok(()) => {
            info!(index = chan_idx, "meshcore channel written");
            // Meshcore's companion protocol does not echo back a
            // ChannelInfo packet after SET_CHANNEL (unlike Meshtastic's
            // admin reply). We synthesise one from what we just wrote so
            // the UI's channels modal updates immediately — matches the
            // user's expectation coming from Meshtastic.
            emit(
                event_tx,
                MeshEvent::ChannelInfo(ChannelInfo {
                    network: Network::Meshcore,
                    index: u32::from(chan_idx),
                    role: ChannelRole::Secondary,
                    name,
                    psk: secret.to_vec(),
                    uplink_enabled: true,
                    downlink_enabled: true,
                }),
            )
            .await;
        }
        Err(e) => {
            warn!(index = chan_idx, error = %e, "meshcore set_channel failed");
            send_err(event_tx, format!("meshcore set_channel: {}", e)).await;
        }
    }
}

/// Parse a 12-lowercase-hex peer id back into its 6 raw bytes.
fn parse_hex_prefix(s: &str) -> Option<[u8; PREFIX_LEN]> {
    if s.len() != PREFIX_LEN * 2 {
        return None;
    }
    let mut out = [0u8; PREFIX_LEN];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
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

    #[test]
    fn hex_prefix_formats_six_bytes() {
        let key = [
            0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a,
            0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
            0x19, 0x1a, 0x1b, 0x1c,
        ];
        assert_eq!(hex_prefix(&key), "deadbeef0102");
    }

    #[test]
    fn hex_prefix_tolerates_short_input() {
        // Defensive: if for some reason we only have 3 bytes of key material,
        // we render what we have instead of panicking on indexing.
        assert_eq!(hex_prefix(&[0xde, 0xad, 0xbe]), "deadbe");
        assert_eq!(hex_prefix(&[]), "");
    }

    #[test]
    fn parse_hex_prefix_roundtrip() {
        let s = "deadbeef0102";
        let bytes = parse_hex_prefix(s).expect("valid");
        assert_eq!(bytes, [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02]);
    }

    #[test]
    fn parse_hex_prefix_rejects_bad_input() {
        assert!(parse_hex_prefix("too short").is_none());
        assert!(parse_hex_prefix("0123456789ab0000").is_none()); // 16 chars, not 12
        assert!(parse_hex_prefix("zzzzzzzzzzzz").is_none()); // not hex
    }

    #[test]
    fn parse_hex_prefix_accepts_upper_case() {
        let a = parse_hex_prefix("deadbeef0102").expect("lower");
        let b = parse_hex_prefix("DEADBEEF0102").expect("upper");
        assert_eq!(a, b);
    }

    #[test]
    fn short_from_long_uses_initials() {
        assert_eq!(short_from_long("Alpha Bravo Charlie Delta Echo"), "ABCD");
        assert_eq!(short_from_long("  spaced    name  "), "sn");
    }

    #[test]
    fn short_from_long_single_word_falls_back_to_prefix() {
        assert_eq!(short_from_long("redlight"), "redl");
        assert_eq!(short_from_long("hi"), "hi");
    }

    #[test]
    fn short_from_long_empty_stays_empty() {
        assert_eq!(short_from_long(""), "");
    }

    #[test]
    fn bytes_hex_matches_hex_prefix_for_six_bytes() {
        let b = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        assert_eq!(bytes_hex(&b), "aabbccddeeff");
        assert_eq!(bytes_hex(&b), hex_prefix(&b));
    }
}
