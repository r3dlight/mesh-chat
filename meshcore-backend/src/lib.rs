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
    Network, NodeInfo, NodeKind, SendStatus,
};
use meshcore_rs::{events::EventPayload, EventType, MeshCore, MeshCoreEvent};
use tokio::sync::{mpsc, Mutex};
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

        // The crate's default timeout is 3s, which is tight for a
        // radio that just finished re-enumerating over USB CDC after
        // our serial open (or after a fresh flash). Bump it to 30s.
        // Why so long: `send_login`/`send_msg` wait for the firmware's
        // `MsgSent` confirmation, and on a radio that's queue-full or
        // duty-cycle-throttled (busy LoRa channel, solar repeater
        // chain) the firmware can sit on the command for 10–20s
        // before acking. We learnt this from `Timeout waiting for
        // Some(MsgSent)` reports against a remote solar repeater.
        mc.set_default_timeout(std::time::Duration::from_secs(30))
            .await;

        // APP_START is mandatory — the radio won't emit any events until
        // the host has identified itself. The response carries the node's
        // public key and advertised name. Retry a couple of times with
        // backoff because the USB CDC endpoint can still be settling
        // right when we open the port; a second APP_START 500ms later
        // usually lands cleanly.
        let self_info = {
            const APPSTART_ATTEMPTS: &[u64] = &[0, 500, 1_200, 2_500];
            let mut last_err: Option<meshcore_rs::Error> = None;
            let mut out = None;
            for (i, delay_ms) in APPSTART_ATTEMPTS.iter().enumerate() {
                if *delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
                }
                match mc.commands().lock().await.send_appstart().await {
                    Ok(info) => {
                        out = Some(info);
                        break;
                    }
                    Err(e) => {
                        warn!(
                            attempt = i + 1,
                            max = APPSTART_ATTEMPTS.len(),
                            error = %e,
                            "meshcore APP_START failed, will retry"
                        );
                        last_err = Some(e);
                    }
                }
            }
            match out {
                Some(info) => info,
                None => {
                    // Close the connection cleanly so the serial port
                    // is released before we propagate the error.
                    if let Err(e) = mc.disconnect().await {
                        debug!(error = %e, "meshcore disconnect after appstart failure");
                    }
                    return Err(anyhow::anyhow!(
                        "meshcore APP_START: all {} attempts failed — {}",
                        APPSTART_ATTEMPTS.len(),
                        last_err
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "unknown error".into())
                    ));
                }
            }
        };
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
                kind: Some(NodeKind::Chat),
            }),
        )
        .await;

        // Push our host system clock to the radio. Meshcore boards
        // without a backed-up RTC drift wildly between reboots, and
        // every OTA admin packet (login especially) carries a
        // timestamp that the remote uses for replay-attack rejection.
        // If our radio's clock is behind whatever timestamp the
        // repeater last recorded for our pubkey, the repeater will
        // silently drop the login. Setting it at connect time mirrors
        // what the official Meshcore phone app does on each session.
        let host_now = chrono::Utc::now().timestamp();
        let host_now_u32 = u32::try_from(host_now).unwrap_or(u32::MAX);
        match mc.commands().lock().await.set_time(host_now_u32).await {
            Ok(_) => info!(
                ts = host_now_u32,
                "meshcore radio clock synced to host time"
            ),
            Err(e) => warn!(
                error = %e,
                "meshcore set_time failed — admin replay-guard checks may reject our requests if the radio clock drifts"
            ),
        }

        // Auto-rebroadcast our full identity to immediate neighbours.
        // Meshcore's firmware auto-advert cadence is long (30 min+) to
        // save airtime, so between two cycles a neighbour that has
        // never heard us will silently drop any DM we send — its
        // firmware discards messages from pubkeys it doesn't have
        // cached. The official Meshcore phone app fires the same call
        // on every companion connect for the same reason. Zero-hop
        // (`flood: false`) keeps the cost to one local-reach packet.
        match mc.commands().lock().await.send_advert(false).await {
            Ok(_) => info!("meshcore auto-advert broadcast on connect"),
            Err(e) => warn!(error = %e, "meshcore auto-advert on connect failed; DMs to fresh neighbours may be dropped until next manual Send Advert"),
        }

        // Populate the sidebar from the firmware's contact cache. Failures
        // here are non-fatal: advertisements will rebuild the list as they
        // arrive from the mesh.
        match mc.commands().lock().await.get_contacts(0).await {
            Ok(contacts) => {
                let named = contacts.iter().filter(|c| !c.adv_name.is_empty()).count();
                let nameless = contacts.len() - named;
                info!(
                    total = contacts.len(),
                    named,
                    nameless,
                    "meshcore contacts loaded"
                );
                if nameless > 0 {
                    warn!(
                        nameless,
                        "some contacts have no name — the firmware only heard path-adverts for them. \
                         Ask each remote to re-broadcast its full identity (CMD_SEND_ADVERT / \
                         `Send advert` in the official client)."
                    );
                }
                for c in &contacts {
                    debug!(
                        prefix = %c.prefix_hex(),
                        name = %c.adv_name,
                        path_len = c.path_len,
                        "meshcore contact"
                    );
                }
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


        // Tracks the peer of the most recent `RepeaterLogin` command that
        // hasn't yet been answered. Meshcore's `LoginSuccess`/`LoginFailed`
        // packets carry no peer attribution, so we correlate by serialising
        // logins (the UI gates the button while one is in flight) and
        // popping the pending peer when the response lands. Wrapped in an
        // Arc<Mutex<>> so both the event-stream task and the command task
        // can touch it.
        let pending_login: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Event pump: meshcore-rs broadcast stream → our mpsc.
        let mc_stream = mc.clone();
        let event_tx_stream = event_tx.clone();
        let my_id_stream = my_id.clone();
        let pending_login_stream = pending_login.clone();
        tokio::spawn(async move {
            let mut stream = mc_stream.event_stream();
            while let Some(ev) = stream.next().await {
                // Login responses don't go through `map_event` because we
                // need access to the pending-login state to attach the
                // peer. Intercept them here, then fall through for
                // everything else.
                // Trace EVERY incoming firmware event so we can verify a
                // login response actually reached us at the meshcore-rs
                // layer. If `ver`/`clock` work but a login times out
                // server-side, this log tells us whether it's:
                //   - a true round-trip drop (no event ever logged), or
                //   - a parsing/wiring bug on our side (event logged but
                //     never surfaced as RepeaterLoginResult).
                info!(
                    event_type = ?ev.event_type,
                    "meshcore raw event"
                );
                match ev.event_type {
                    EventType::LoginSuccess | EventType::LoginFailed => {
                        let peer = pending_login_stream.lock().await.take();
                        if let Some(peer) = peer {
                            let ok = matches!(ev.event_type, EventType::LoginSuccess);
                            info!(
                                %peer,
                                ok,
                                "repeater login response correlated to pending peer"
                            );
                            emit(
                                &event_tx_stream,
                                MeshEvent::RepeaterLoginResult {
                                    network: Network::Meshcore,
                                    peer,
                                    ok,
                                    error: if ok {
                                        None
                                    } else {
                                        Some("repeater rejected the password".into())
                                    },
                                },
                            )
                            .await;
                        } else {
                            warn!(
                                event_type = ?ev.event_type,
                                "login response arrived with no pending login — dropped"
                            );
                        }
                        continue;
                    }
                    _ => {}
                }
                for mapped in map_event(ev, &my_id_stream) {
                    emit(&event_tx_stream, mapped).await;
                }
            }
            debug!("meshcore event stream closed");
        });

        // Command loop: translate our generic commands to meshcore-rs calls.
        let mc_cmd = mc.clone();
        let event_tx_cmd = event_tx.clone();
        let my_id_cmd = my_id.clone();
        let pending_login_cmd = pending_login.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if matches!(cmd, MeshCommand::Shutdown) {
                    info!("meshcore shutdown requested");
                    if let Err(e) = mc_cmd.disconnect().await {
                        warn!(error = %e, "meshcore disconnect failed");
                    }
                    break;
                }
                handle_cmd(&mc_cmd, cmd, &event_tx_cmd, &my_id_cmd, &pending_login_cmd).await;
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
    // Scrub garbage names from the firmware contact cache too. Normally
    // these come through fine (the firmware stores the clean UTF-8
    // string), but if we ever get a corrupted entry we don't want it
    // to poison the sidebar.
    let clean_name = if is_plausible_name(&c.adv_name) {
        c.adv_name.clone()
    } else {
        String::new()
    };
    MeshEvent::NodeSeen(NodeInfo {
        network: Network::Meshcore,
        id: c.prefix_hex(),
        long_name: clean_name.clone(),
        short_name: short_from_long(&clean_name),
        battery_level: None,
        voltage: None,
        snr: None,
        // `c.last_advert` comes from the firmware's contact cache.
        // On ESP32 targets without a persistent RTC it's often an
        // uptime-since-boot counter rather than a real unix epoch,
        // which made the UI show nonsense like "709 days ago". Only
        // accept it if it looks plausibly recent (within ± 1 year
        // of our wall clock) — otherwise leave it None and let the
        // live observation path (Advertisement / incoming message
        // events) populate `last_heard` with our own `Utc::now()`
        // when we actually hear from the peer.
        last_heard: plausible_unix_seconds(c.last_advert),
        hops_away: if c.path_len < 0 {
            None
        } else {
            Some(c.path_len as u32)
        },
        kind: Some(NodeKind::from_meshcore_byte(c.contact_type)),
    })
}

/// Returns true if `name` looks like a human-typed identifier rather
/// than raw binary bytes surfaced as mojibake. Guards against a bug in
/// `meshcore-rs` 0.1.10: its `Advertisement` parser reads 32 bytes at
/// offset 6 of the packet payload as the name, but those bytes are
/// really the timestamp + signature fields — the actual name lives
/// further in. Result: `a.name` on full-identity adverts regularly
/// contains binary garbage that `String::from_utf8_lossy` turns into
/// `U+FFFD` replacement chars. We reject any name containing
/// replacement chars, control chars, or a high ratio of non-printables.
fn is_plausible_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return false;
    }
    // U+FFFD is the replacement char — its presence means lossy UTF-8
    // decode found invalid bytes, which is how binary slop shows up.
    if trimmed.contains('\u{FFFD}') {
        return false;
    }
    let mut total = 0usize;
    let mut suspicious = 0usize;
    for ch in trimmed.chars() {
        total += 1;
        // Any C0 control char (except tab) is a strong signal of binary data.
        if (ch.is_control() && ch != '\t') || ch == '\u{FEFF}' {
            return false;
        }
        // Count characters that are neither letters, digits, nor common
        // punctuation/emoji as "suspicious". Pure binary tends to be
        // ~100% suspicious; a real node name is ~0%.
        if !(ch.is_alphanumeric()
            || ch.is_whitespace()
            || "-_./:()[]!?@+*#&',\"".contains(ch)
            || ch as u32 >= 0x2000) // emoji / ext punctuation
        {
            suspicious += 1;
        }
    }
    // Tolerate up to 25% suspicious chars — real names sometimes have odd
    // symbols, binary slop is dense.
    suspicious * 4 <= total
}

/// Accepts a u32 "seconds" value only if it looks like a unix epoch
/// within ± 1 year of now; otherwise returns `None`. Defends against
/// firmware that exposes uptime-since-boot under a timestamp name.
fn plausible_unix_seconds(secs: u32) -> Option<i64> {
    if secs == 0 {
        return None;
    }
    let ts = i64::from(secs);
    let now = chrono::Utc::now().timestamp();
    const ONE_YEAR_SECS: i64 = 365 * 24 * 3600;
    if (now - ts).abs() <= ONE_YEAR_SECS {
        Some(ts)
    } else {
        None
    }
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
            // Drop firmware echoes of our own outgoing messages. Some
            // Meshcore firmware versions replay every queued send back
            // to the companion as `ContactMsgRecv` with sender_prefix =
            // our own pubkey, presumably as a "yes I sent it" cue. We
            // already render outgoing DMs through `send_text`'s
            // synthetic local echo, so accepting these would double
            // the bubble in the chat thread.
            let sender = msg.sender_prefix_hex();
            if sender == my_id {
                info!(
                    %sender,
                    "dropping firmware echo of our own ContactMsg (already rendered via local echo)"
                );
                return vec![];
            }
            info!(
                %sender,
                text_len = msg.text.len(),
                "ContactMsgRecv → TextMessage (incoming DM from peer)"
            );
            vec![MeshEvent::TextMessage(ChatMessage {
                timestamp: chrono::Utc::now().timestamp(),
                network: Network::Meshcore,
                channel: 0,
                from: sender,
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
            // Two kinds of adverts on the Meshcore air:
            // - "full-identity" adverts carry the node's advertised
            //   name, coords etc.
            // - "path-only" adverts carry just the pubkey prefix and
            //   are used for route discovery.
            //
            // We only surface the full ones to the UI as `NodeSeen`.
            // Path-only adverts (empty `name`) would otherwise flood
            // the Nodes modal with anonymous `…xxxx` entries the
            // user can't DM anyway — and on a busy mesh they arrive
            // every few seconds.
            let mut out = vec![];
            if let Some(pos) = position_from_advertisement(&a) {
                out.push(pos);
            }
            if is_plausible_name(&a.name) {
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
                    // `AdvertisementData` doesn't carry contact_type in
                    // meshcore-rs 0.1.10. The contact-cache path
                    // (`contact_to_node`) will fill this in once the
                    // firmware stores the peer, so `None` here is fine.
                    kind: None,
                }));
            } else {
                debug!(
                    prefix = %bytes_hex(&a.prefix),
                    name_bytes = a.name.len(),
                    "advert dropped — name empty or looks like binary (meshcore-rs 0.1.10 advert parser offset bug)"
                );
            }
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

async fn handle_cmd(
    mc: &Arc<MeshCore>,
    cmd: MeshCommand,
    event_tx: &mpsc::Sender<MeshEvent>,
    my_id: &str,
    pending_login: &Arc<Mutex<Option<String>>>,
) {
    match cmd {
        MeshCommand::SendText {
            local_id,
            channel,
            text,
            to,
        } => send_text(mc, local_id, channel, text, to, event_tx, my_id).await,
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
        MeshCommand::RefreshNodes => {
            // Re-query the firmware's contact cache. A remote rename
            // only propagates to the local radio on the next advert
            // from that remote (≈30 min cycle), so this can still
            // show stale data — but at least it surfaces anything
            // that landed between our startup sweep and now.
            match mc.commands().lock().await.get_contacts(0).await {
                Ok(contacts) => {
                    debug!(count = contacts.len(), "meshcore contacts refreshed");
                    for c in contacts {
                        emit(event_tx, contact_to_node(&c)).await;
                        if let Some(pos) = position_from_contact(&c) {
                            emit(event_tx, pos).await;
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "meshcore refresh get_contacts failed");
                    send_err(event_tx, format!("meshcore refresh: {}", e)).await;
                }
            }
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
        MeshCommand::ForgetNode { id } => {
            let prefix_bytes = match parse_hex_prefix(&id) {
                Some(b) => b,
                None => {
                    send_err(
                        event_tx,
                        format!("invalid node id {}: expect 12 hex chars", id),
                    )
                    .await;
                    return;
                }
            };
            // meshcore-rs 0.1.10's `remove_contact()` sends CMD_REMOVE_CONTACT
            // followed by only the 6-byte prefix, but the wire format (per
            // its own doc comment) is `[0x0F][pubkey: 32]`. Firmware silently
            // keeps the contact when it gets 6 bytes instead of 32, so the
            // "forget" looks successful while the cache stays untouched.
            // Bypass the crate and issue the raw command ourselves with the
            // full 32-byte public key.
            let contact = mc.get_contact_by_prefix(&prefix_bytes).await;
            let Some(contact) = contact else {
                send_err(
                    event_tx,
                    format!("forget node: no known Meshcore contact with prefix {}", id),
                )
                .await;
                return;
            };
            // CMD_REMOVE_CONTACT = 0x0F. Wait for either `Ok` (firmware
            // removed the contact) or `Error` (firmware couldn't find /
            // remove it). Without `send_multi` we'd block on `Ok` only
            // and the user-facing forget would silently time out at 30s
            // when the firmware actually did respond — just with an
            // error frame rather than the OK frame meshcore-rs expected.
            let mut raw = Vec::with_capacity(1 + 32);
            raw.push(0x0F);
            raw.extend_from_slice(&contact.public_key);
            let result = mc
                .commands()
                .lock()
                .await
                .send_multi(
                    &raw,
                    &[meshcore_rs::EventType::Ok, meshcore_rs::EventType::Error],
                    std::time::Duration::from_secs(15),
                )
                .await;
            match result {
                Ok(ev) if ev.event_type == meshcore_rs::EventType::Ok => {
                    info!(%id, pubkey_prefix = %bytes_hex(&contact.public_key[..6]), "meshcore CMD_REMOVE_CONTACT acked OK by firmware");
                    // Refresh meshcore-rs's in-memory contact cache so a
                    // subsequent `send_msg` doesn't find a stale entry
                    // for the deleted peer. The library only re-populates
                    // on explicit `get_contacts()`.
                    match mc.commands().lock().await.get_contacts(0).await {
                        Ok(contacts) => {
                            debug!(
                                count = contacts.len(),
                                "meshcore contact cache rehydrated after forget"
                            );
                        }
                        Err(e) => {
                            warn!(error = %e, "get_contacts after remove failed (non-fatal)");
                        }
                    }
                    emit(
                        event_tx,
                        MeshEvent::NodeRemoved {
                            network: Network::Meshcore,
                            id,
                        },
                    )
                    .await;
                }
                Ok(ev) => {
                    // EventType::Error — firmware couldn't find the contact
                    // by the pubkey we sent. This typically means our
                    // copy of `contact.public_key` is partial/zero (the
                    // firmware only ever heard a path-advert for this
                    // peer, no full identity), or the contact was
                    // already gone.
                    let detail = match ev.payload {
                        meshcore_rs::events::EventPayload::String(s) => s,
                        _ => "ERR_NOT_FOUND or unable to remove".into(),
                    };
                    warn!(
                        %id,
                        pubkey_prefix = %bytes_hex(&contact.public_key[..6]),
                        pubkey_zero = contact.public_key.iter().all(|&b| b == 0),
                        %detail,
                        "meshcore CMD_REMOVE_CONTACT rejected by firmware"
                    );
                    send_err(
                        event_tx,
                        format!(
                            "forget node {}: firmware rejected — {} (often: only a path-advert was heard for this peer, full pubkey unknown)",
                            id, detail
                        ),
                    )
                    .await;
                }
                Err(e) => {
                    warn!(%id, error = %e, "meshcore remove_contact raw send failed");
                    send_err(event_tx, format!("forget node: {}", e)).await;
                }
            }
        }
        MeshCommand::SendAdvert { flood } => {
            // Rebroadcast our full identity so neighbours add us to their
            // contact cache. Critical for DMs: Meshcore silently drops
            // messages signed by an unknown sender pubkey, so if the
            // remote never heard our advert, nothing we send will render
            // on their screen.
            match mc.commands().lock().await.send_advert(flood).await {
                Ok(_) => {
                    info!(flood, "meshcore advert broadcast accepted");
                    send_err(
                        event_tx,
                        if flood {
                            "advert broadcast (flood) — give neighbours ~10s to cache our identity".into()
                        } else {
                            "advert broadcast (zero-hop) — immediate neighbours only".into()
                        },
                    )
                    .await;
                }
                Err(e) => {
                    warn!(error = %e, "meshcore send_advert failed");
                    send_err(event_tx, format!("send advert: {}", e)).await;
                }
            }
        }
        MeshCommand::RepeaterLogin { peer, password } => {
            let prefix_bytes = match parse_hex_prefix(&peer) {
                Some(b) => b,
                None => {
                    send_err(
                        event_tx,
                        format!("invalid peer id {}: expect 12 hex chars", peer),
                    )
                    .await;
                    return;
                }
            };
            let Some(contact) = mc.get_contact_by_prefix(&prefix_bytes).await else {
                send_err(
                    event_tx,
                    format!("login: no Meshcore contact with prefix {}", peer),
                )
                .await;
                return;
            };
            // Refuse to overwrite a pending login: the firmware emits
            // unattributed `LoginSuccess`/`LoginFailed` and we'd
            // otherwise misroute the response.
            {
                let mut slot = pending_login.lock().await;
                if slot.is_some() {
                    send_err(
                        event_tx,
                        "another repeater login is already in flight — wait for the response".into(),
                    )
                    .await;
                    return;
                }
                *slot = Some(peer.clone());
            }
            match mc
                .commands()
                .lock()
                .await
                .send_login(contact, &password)
                .await
            {
                Ok(info) => {
                    info!(
                        %peer,
                        expected_ack = ?info.expected_ack,
                        "meshcore login request accepted by radio (awaiting LoginSuccess/Failed)"
                    );
                    // Watchdog: the repeater's LoginSuccess/Failed comes
                    // through the event stream asynchronously, but on a
                    // distant or solar-powered repeater it can never
                    // arrive (no LoS, asleep, wrong path). Without a
                    // safety net the UI button would stay stuck on
                    // "Authenticating…" forever. After 45 s we evict
                    // the pending slot and surface a timeout — the user
                    // gets a clear failure they can act on.
                    let pending_login_watchdog = pending_login.clone();
                    let event_tx_watchdog = event_tx.clone();
                    let peer_watchdog = peer.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
                        let mut slot = pending_login_watchdog.lock().await;
                        if slot.as_deref() == Some(peer_watchdog.as_str()) {
                            *slot = None;
                            drop(slot);
                            warn!(%peer_watchdog, "login response timed out (no LoginSuccess/Failed in 45s)");
                            emit(
                                &event_tx_watchdog,
                                MeshEvent::RepeaterLoginResult {
                                    network: Network::Meshcore,
                                    peer: peer_watchdog,
                                    ok: false,
                                    error: Some(
                                    "no LoginSuccess/Failed in 45s. MeshCore repeaters silently drop wrong passwords — most likely cause: \
                                     bad password (default build = \"password\"), our pubkey not in the repeater's ACL, \
                                     or local clock behind the repeater's last-seen timestamp for us (NTP fix)."
                                        .into(),
                                ),
                                },
                            )
                            .await;
                        }
                    });
                }
                Err(e) => {
                    warn!(%peer, error = %e, "meshcore send_login failed");
                    *pending_login.lock().await = None;
                    emit(
                        event_tx,
                        MeshEvent::RepeaterLoginResult {
                            network: Network::Meshcore,
                            peer,
                            ok: false,
                            error: Some(format!("send login: {}", e)),
                        },
                    )
                    .await;
                }
            }
        }
        MeshCommand::RepeaterLogout { peer } => {
            let prefix_bytes = match parse_hex_prefix(&peer) {
                Some(b) => b,
                None => {
                    send_err(
                        event_tx,
                        format!("invalid peer id {}: expect 12 hex chars", peer),
                    )
                    .await;
                    return;
                }
            };
            let Some(contact) = mc.get_contact_by_prefix(&prefix_bytes).await else {
                send_err(
                    event_tx,
                    format!("logout: no Meshcore contact with prefix {}", peer),
                )
                .await;
                return;
            };
            match mc.commands().lock().await.send_logout(contact).await {
                Ok(_) => {
                    info!(%peer, "meshcore logout sent");
                    // Reuse RepeaterLoginResult with ok=false to signal
                    // "session no longer active" — the UI flips its
                    // logged-in flag on either kind of result.
                    emit(
                        event_tx,
                        MeshEvent::RepeaterLoginResult {
                            network: Network::Meshcore,
                            peer,
                            ok: false,
                            error: Some("logged out".into()),
                        },
                    )
                    .await;
                }
                Err(e) => {
                    warn!(%peer, error = %e, "meshcore send_logout failed");
                    send_err(event_tx, format!("logout: {}", e)).await;
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
    my_id: &str,
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
                format!(
                    "no known Meshcore contact with prefix {} — hit 🔄 Refresh in Nodes, or have the remote re-advertise",
                    peer
                ),
            )
            .await;
            return;
        };
        // Guard against sending to a half-known contact. If the firmware
        // only ever heard a path-advert and never a full-identity advert
        // for this peer, its public_key field will be zero-filled — which
        // means pubkey-based encryption would send gibberish nobody can
        // decrypt. Surface that clearly instead of silently emitting a
        // dead message the remote will never see.
        if contact.public_key.iter().all(|&b| b == 0) {
            send_failure(
                event_tx,
                local_id,
                format!(
                    "contact {} has no full identity yet — ask the remote to Send Advert and retry",
                    peer
                ),
            )
            .await;
            return;
        }
        let name = contact.adv_name.clone();
        let peer_id = peer.clone();
        let send_text = text.clone();
        let path_len = contact.path_len;
        let pubkey_prefix = bytes_hex(&contact.public_key[..6]);
        match mc.commands().lock().await.send_msg(contact, &text, None).await {
            Ok(info) => {
                info!(
                    local_id,
                    peer = %name,
                    peer_id = %pubkey_prefix,
                    path_len,
                    expected_ack = ?info.expected_ack,
                    suggested_timeout_ms = info.suggested_timeout,
                    "meshcore DM accepted by radio"
                );
                if path_len < 0 {
                    // No cached route — the firmware will flood the message.
                    // Surface this so the user understands why delivery may
                    // take 10–30 s the first time (and may fail if the
                    // remote radio isn't within flood range).
                    send_err(
                        event_tx,
                        format!(
                            "no known path to {} yet — message flooded; first DM can take 10-30s, after that the path caches",
                            name
                        ),
                    )
                    .await;
                }
                // Local echo: Meshcore's companion protocol doesn't replay
                // our own transmit back to us (unlike Meshtastic's
                // PacketRouter), so the UI needs a synthetic TextMessage
                // to render the bubble in our own DM thread.
                info!(
                    local_id,
                    peer = %peer_id,
                    text_len = send_text.len(),
                    "emitting DM local-echo (1 per send_text call)"
                );
                emit(
                    event_tx,
                    MeshEvent::TextMessage(ChatMessage {
                        timestamp: chrono::Utc::now().timestamp(),
                        network: Network::Meshcore,
                        channel: 0,
                        from: my_id.to_string(),
                        to: peer_id,
                        text: send_text,
                        local_id: Some(local_id),
                        status: Some(SendStatus::Sent),
                        rx_snr: None,
                        rx_rssi: None,
                        reply_to_text: None,
                        packet_id: None,
                        reactions: std::collections::HashMap::new(),
                    }),
                )
                .await;
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
    let channel_text = text.clone();
    match mc
        .commands()
        .lock()
        .await
        .send_channel_msg(chan_idx, &text, None)
        .await
    {
        Ok(()) => {
            info!(local_id, channel = chan_idx, "meshcore channel msg accepted");
            // Local echo for channel sends — same reasoning as DMs.
            emit(
                event_tx,
                MeshEvent::TextMessage(ChatMessage {
                    timestamp: chrono::Utc::now().timestamp(),
                    network: Network::Meshcore,
                    channel: u32::from(chan_idx),
                    from: my_id.to_string(),
                    to: "^all".into(),
                    text: channel_text,
                    local_id: Some(local_id),
                    status: Some(SendStatus::Sent),
                    rx_snr: None,
                    rx_rssi: None,
                    reply_to_text: None,
                    packet_id: None,
                    reactions: std::collections::HashMap::new(),
                }),
            )
            .await;
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

    #[test]
    fn plausible_name_accepts_normal_names() {
        assert!(is_plausible_name("redble"));
        assert!(is_plausible_name("Alpha Bravo"));
        assert!(is_plausible_name("node-42"));
        assert!(is_plausible_name("chez_moi"));
        assert!(is_plausible_name("📡 relay"));
    }

    #[test]
    fn plausible_name_rejects_binary_slop() {
        // The exact symptom reported: meshcore-rs 0.1.10 decodes
        // signature bytes as lossy UTF-8, producing replacement chars.
        let mojibake = "\u{FFFD}\u{FFFD}}\u{FFFD}N\"\u{FFFD}\u{FFFD}]l\u{FFFD}t\u{FFFD}A\u{FFFD}\u{FFFD}{\u{FFFD}m6g)\u{FFFD}\u{FFFD}";
        assert!(!is_plausible_name(mojibake));
        // Raw binary re-decoded — should also be rejected.
        let raw = String::from_utf8_lossy(&[0xff, 0x12, 0x00, 0x7d, 0x4e]).to_string();
        assert!(!is_plausible_name(&raw));
    }

    #[test]
    fn plausible_name_rejects_empty_and_whitespace() {
        assert!(!is_plausible_name(""));
        assert!(!is_plausible_name("   "));
        assert!(!is_plausible_name("\t\n"));
    }
}
