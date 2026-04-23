//! Meshtastic backend: talks to Meshtastic firmware over serial (protobuf).
//!
//! The upstream API (`meshtastic` crate, v0.1.8) requires a `PacketRouter`
//! to send packets — we provide a minimal one that just carries the local
//! node id (populated once we receive the MyInfo packet).

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use mesh_core::{
    BackendHandle, ChannelInfo, ChannelRole, ChatMessage, DeviceRoleInfo, LoraInfo, MeshBackend,
    MeshCommand, MeshEvent, Network, NodeInfo,
};
use meshtastic::packet::{PacketDestination, PacketRouter};
use meshtastic::protobufs;
use meshtastic::types::{MeshChannel, NodeId};
use tokio::sync::mpsc;
use tracing::{debug, info, instrument, warn};

/// How long we keep a packet_id → local_id mapping waiting for a Routing
/// ack before discarding it. Meshtastic's default flood-routing retry
/// window is ~1 min; we go generous to cover slow links without leaking.
const ACK_PENDING_TTL: Duration = Duration::from_secs(300);

/// Snapshot of the last radio-pushed config packets, kept so that
/// subsequent writes can overlay only the fields the user changed
/// instead of resetting every unspecified field to its proto default.
#[derive(Default)]
struct ConfigCache {
    lora: Option<protobufs::config::LoRaConfig>,
    device: Option<protobufs::config::DeviceConfig>,
}

/// Encodes a single channel as a Meshtastic share URL:
/// `https://meshtastic.org/e/#<base64url>` where the payload is a
/// `ChannelSet` protobuf containing just this channel's settings and a
/// minimal LoRaConfig. Scanning the QR code on another device imports
/// the channel (the recipient's region/preset stays untouched because
/// we leave LoRaConfig empty).
pub fn encode_channel_share_url(channel: &ChannelInfo) -> Result<String> {
    use base64::Engine;
    use prost::Message;

    let mut settings = protobufs::ChannelSettings {
        psk: channel.psk.clone(),
        name: channel.name.clone(),
        id: 0,
        uplink_enabled: channel.uplink_enabled,
        downlink_enabled: channel.downlink_enabled,
        module_settings: None,
        ..Default::default()
    };
    // `channel_num` is deprecated but still present; leave at its default.
    #[allow(deprecated)]
    {
        settings.channel_num = 0;
    }
    let set = protobufs::ChannelSet {
        settings: vec![settings],
        lora_config: None,
    };
    let mut buf = Vec::with_capacity(set.encoded_len());
    set.encode(&mut buf)
        .map_err(|e| anyhow::anyhow!("protobuf encode: {}", e))?;
    // URL-safe base64 without padding — matches meshtastic.org/e/#
    // convention used by the mobile apps.
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf);
    Ok(format!("https://meshtastic.org/e/#{}", encoded))
}

/// Parse a node id of the form `"!abcd1234"` into its u32 numeric form.
fn parse_node_id(s: &str) -> Result<u32> {
    let trimmed = s.strip_prefix('!').unwrap_or(s);
    u32::from_str_radix(trimmed, 16)
        .map_err(|e| anyhow::anyhow!("node id must be 8 hex chars: {}", e))
}

/// Minimal router: the `send_text` API needs one to set the `from` field on
/// outgoing packets. We also use the echo-response hook to capture the
/// packet id the upstream crate generated on our behalf — that id is the
/// correlator used by Routing ACKs coming back from the radio.
struct LocalRouter {
    my_id: NodeId,
    /// Packet id of the last outgoing mesh packet, written from inside
    /// `handle_mesh_packet` (called synchronously by `send_mesh_packet`
    /// when `echo_response` is true). The caller reads-and-clears it
    /// immediately after the `send_*` await resolves.
    last_sent_packet_id: Option<u32>,
}

impl PacketRouter<(), Infallible> for LocalRouter {
    fn handle_packet_from_radio(
        &mut self,
        _packet: protobufs::FromRadio,
    ) -> std::result::Result<(), Infallible> {
        Ok(())
    }

    fn handle_mesh_packet(
        &mut self,
        packet: protobufs::MeshPacket,
    ) -> std::result::Result<(), Infallible> {
        self.last_sent_packet_id = Some(packet.id);
        Ok(())
    }

    fn source_node_id(&self) -> NodeId {
        self.my_id
    }
}

pub struct MeshtasticBackend {
    serial_port: String,
}

impl MeshtasticBackend {
    pub fn new(serial_port: impl Into<String>) -> Self {
        Self {
            serial_port: serial_port.into(),
        }
    }
}

#[async_trait]
impl MeshBackend for MeshtasticBackend {
    fn network(&self) -> Network {
        Network::Meshtastic
    }

    #[instrument(skip(self), fields(port = %self.serial_port))]
    async fn start(&self) -> Result<BackendHandle> {
        use meshtastic::api::StreamApi;
        use meshtastic::utils;

        let (event_tx, event_rx) = mpsc::channel::<MeshEvent>(256);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<MeshCommand>(64);

        let stream_api = StreamApi::new();
        // Opening USB-CDC ports on ESP32-based boards can fail with
        // "Broken pipe" when the toggle of DTR (default=true) triggers a
        // chip reset — the USB bridge briefly disconnects mid-configure
        // and subsequent pin writes error out.
        //
        // Strategy:
        // - First 2 attempts with default (DTR=true, RTS=false): reset the
        //   chip into a clean state. Wait 1.2s between tries so the ESP
        //   has time to re-enumerate.
        // - Last 3 attempts with DTR=false, RTS=false: skip pin toggling
        //   entirely. If the chip is already in normal Meshtastic mode,
        //   this just opens the port without disturbing it.
        let serial_stream = {
            const ATTEMPTS: &[(Option<bool>, Option<bool>, u64)] = &[
                (None, None, 1_200), // default DTR/RTS (reset), wait 1.2s
                (None, None, 1_500), // retry reset, wait 1.5s
                (Some(false), Some(false), 500), // stop toggling pins
                (Some(false), Some(false), 1_000),
                (Some(false), Some(false), 2_000),
            ];
            let max_attempts = ATTEMPTS.len();
            let mut last_err: Option<anyhow::Error> = None;
            let mut out = None;
            for (i, (dtr, rts, wait_ms)) in ATTEMPTS.iter().enumerate() {
                let attempt = i + 1;
                match utils::stream::build_serial_stream(
                    self.serial_port.clone(),
                    None,
                    *dtr,
                    *rts,
                ) {
                    Ok(s) => {
                        info!(
                            attempt,
                            dtr = ?dtr,
                            rts = ?rts,
                            "serial opened"
                        );
                        out = Some(s);
                        break;
                    }
                    Err(e) => {
                        warn!(
                            attempt,
                            max = max_attempts,
                            error = %e,
                            dtr = ?dtr,
                            rts = ?rts,
                            port = %self.serial_port,
                            "serial open failed, retrying"
                        );
                        last_err = Some(anyhow::anyhow!(e));
                        tokio::time::sleep(std::time::Duration::from_millis(*wait_ms)).await;
                    }
                }
            }
            match out {
                Some(s) => s,
                None => {
                    return Err(last_err.unwrap_or_else(|| {
                        anyhow::anyhow!(
                            "serial open: all {} attempts failed — unplug the radio, replug, then retry",
                            max_attempts
                        )
                    }));
                }
            }
        };

        let (mut decoded_listener, stream_api) = stream_api.connect(serial_stream).await;
        let config_id = utils::generate_rand_id();
        let stream_api = stream_api.configure(config_id).await?;
        info!(port = %self.serial_port, network = Network::Meshtastic.as_str(), "backend started");

        tokio::spawn(async move {
            let mut stream_api = Some(stream_api);
            let mut router = LocalRouter {
                my_id: NodeId::from(0u32),
                last_sent_packet_id: None,
            };
            // packet_id → (local_id, sent_at). Populated when we send a text
            // with want_ack; consumed when the matching Routing packet comes
            // back. Stale entries are pruned on every Routing arrival.
            let mut pending_acks: HashMap<u32, (u64, Instant)> = HashMap::new();
            let mut config_cache = ConfigCache::default();

            loop {
                tokio::select! {
                    maybe_packet = decoded_listener.recv() => {
                        match maybe_packet {
                            Some(packet) => handle_packet(packet, &event_tx, &mut router, &mut pending_acks, &mut config_cache).await,
                            None => {
                                debug!(network = Network::Meshtastic.as_str(), "serial stream closed by radio");
                                emit(&event_tx, MeshEvent::Disconnected { network: Network::Meshtastic }).await;
                                break;
                            }
                        }
                    }
                    maybe_cmd = cmd_rx.recv() => {
                        let Some(cmd) = maybe_cmd else { break };
                        match cmd {
                            MeshCommand::SendText { local_id, channel, text, to } => {
                                let Some(api) = stream_api.as_mut() else { continue };
                                send_text(api, &mut router, &mut pending_acks, local_id, channel, text, to, &event_tx).await;
                            }
                            MeshCommand::SetChannel { index, role, name, psk } => {
                                let Some(api) = stream_api.as_mut() else { continue };
                                set_channel(api, &mut router, index, role, name, psk, &event_tx).await;
                            }
                            MeshCommand::UpdateUser { long_name, short_name } => {
                                let Some(api) = stream_api.as_mut() else { continue };
                                update_user(api, &mut router, long_name, short_name, &event_tx).await;
                            }
                            MeshCommand::SetLoraConfig { region, modem_preset, use_preset, hop_limit, tx_enabled, tx_power } => {
                                let Some(api) = stream_api.as_mut() else { continue };
                                set_lora_config(
                                    api, &mut router, &config_cache, region, modem_preset, use_preset,
                                    hop_limit, tx_enabled, tx_power, &event_tx,
                                ).await;
                            }
                            MeshCommand::SetDeviceRole { role } => {
                                let Some(api) = stream_api.as_mut() else { continue };
                                set_device_role(api, &mut router, &config_cache, role, &event_tx).await;
                            }
                            MeshCommand::SendReaction { local_id, channel, to, reply_to_packet_id, emoji } => {
                                let Some(api) = stream_api.as_mut() else { continue };
                                send_reaction(
                                    api, &mut router, &mut pending_acks,
                                    local_id, channel, to, reply_to_packet_id, emoji,
                                    &event_tx,
                                ).await;
                            }
                            MeshCommand::SendPosition { local_id, latitude, longitude } => {
                                let Some(api) = stream_api.as_mut() else { continue };
                                send_position(
                                    api, &mut router, &mut pending_acks,
                                    local_id, latitude, longitude, &event_tx,
                                ).await;
                            }
                            MeshCommand::Shutdown => {
                                info!(network = Network::Meshtastic.as_str(), "shutdown requested");
                                if let Some(api) = stream_api.take() {
                                    if let Err(e) = api.disconnect().await {
                                        warn!(error = %e, "disconnect failed");
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(BackendHandle {
            events: event_rx,
            commands: cmd_tx,
        })
    }
}

#[instrument(
    skip(api, router, pending_acks, event_tx, text, to),
    fields(channel, bytes = text.len(), local_id, dm = to.is_some())
)]
#[allow(clippy::too_many_arguments)]
async fn send_text(
    api: &mut meshtastic::api::ConnectedStreamApi<meshtastic::api::state::Configured>,
    router: &mut LocalRouter,
    pending_acks: &mut HashMap<u32, (u64, Instant)>,
    local_id: u64,
    channel: u32,
    text: String,
    to: Option<String>,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    let chan = match MeshChannel::new(channel) {
        Ok(c) => c,
        Err(e) => {
            warn!(channel, error = %e, "invalid channel index");
            let err = format!("invalid channel {}: {}", channel, e);
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: false,
                    error: Some(err.clone()),
                },
            )
            .await;
            send_err(event_tx, err).await;
            return;
        }
    };

    let destination = match to.as_deref() {
        None => PacketDestination::Broadcast,
        Some(id_str) => match parse_node_id(id_str) {
            Ok(num) => PacketDestination::Node(NodeId::from(num)),
            Err(e) => {
                warn!(to = %id_str, error = %e, "invalid DM target");
                let err = format!("invalid DM target {}: {}", id_str, e);
                emit(
                    event_tx,
                    MeshEvent::SendResult {
                        network: Network::Meshtastic,
                        local_id,
                        ok: false,
                        error: Some(err.clone()),
                    },
                )
                .await;
                send_err(event_tx, err).await;
                return;
            }
        },
    };

    // Clear any stale echo before we send so the post-send read only sees
    // this packet's id.
    router.last_sent_packet_id = None;
    let result = api.send_text(router, text, destination, true, chan).await;

    match result {
        Ok(()) => {
            // The upstream crate called `router.handle_mesh_packet` with the
            // freshly-built MeshPacket (because send_text sets
            // echo_response: true). Pick up the generated packet id so we
            // can correlate the eventual Routing ACK.
            if let Some(pkt_id) = router.last_sent_packet_id.take() {
                pending_acks.insert(pkt_id, (local_id, Instant::now()));
                debug!(channel, local_id, packet_id = pkt_id, "awaiting routing ack");
            } else {
                warn!(
                    local_id,
                    "send_text echoed no packet id — ack correlation disabled for this message"
                );
            }
            info!(channel, local_id, "text sent");
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: true,
                    error: None,
                },
            )
            .await;
        }
        Err(e) => {
            let err = format!("send: {}", e);
            warn!(channel, local_id, error = %e, "text send failed");
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: false,
                    error: Some(err.clone()),
                },
            )
            .await;
            send_err(event_tx, err).await;
        }
    }
}

/// Send an emoji reaction. Wraps `send_mesh_packet` with `portnum=TextMessageApp`,
/// `emoji=1` (the flag telling the firmware to treat the payload as a reaction
/// rather than a new text bubble) and `reply_id` pointing at the target packet.
/// Routing-ack correlation works the same way as `send_text` — we capture the
/// echoed packet id via the PacketRouter hook and insert it into `pending_acks`.
#[instrument(
    skip(api, router, pending_acks, event_tx, emoji),
    fields(channel, local_id, reply_to_packet_id, dm = to.is_some())
)]
#[allow(clippy::too_many_arguments)]
async fn send_reaction(
    api: &mut meshtastic::api::ConnectedStreamApi<meshtastic::api::state::Configured>,
    router: &mut LocalRouter,
    pending_acks: &mut HashMap<u32, (u64, Instant)>,
    local_id: u64,
    channel: u32,
    to: Option<String>,
    reply_to_packet_id: u32,
    emoji: String,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    let chan = match MeshChannel::new(channel) {
        Ok(c) => c,
        Err(e) => {
            let err = format!("invalid channel {}: {}", channel, e);
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: false,
                    error: Some(err.clone()),
                },
            )
            .await;
            send_err(event_tx, err).await;
            return;
        }
    };

    let destination = match to.as_deref() {
        None => PacketDestination::Broadcast,
        Some(id_str) => match parse_node_id(id_str) {
            Ok(num) => PacketDestination::Node(NodeId::from(num)),
            Err(e) => {
                let err = format!("invalid DM target {}: {}", id_str, e);
                emit(
                    event_tx,
                    MeshEvent::SendResult {
                        network: Network::Meshtastic,
                        local_id,
                        ok: false,
                        error: Some(err.clone()),
                    },
                )
                .await;
                send_err(event_tx, err).await;
                return;
            }
        },
    };

    let payload: meshtastic::types::EncodedMeshPacketData = emoji.into_bytes().into();
    router.last_sent_packet_id = None;
    let result = api
        .send_mesh_packet(
            router,
            payload,
            protobufs::PortNum::TextMessageApp,
            destination,
            chan,
            /* want_ack */ true,
            /* want_response */ false,
            /* echo_response */ true,
            Some(reply_to_packet_id),
            // `emoji` field is a u32 flag on the proto — any non-zero means
            // "treat this packet as an emoji reaction". We set it to 1.
            Some(1),
        )
        .await;

    match result {
        Ok(()) => {
            if let Some(pkt_id) = router.last_sent_packet_id.take() {
                pending_acks.insert(pkt_id, (local_id, Instant::now()));
                debug!(channel, local_id, packet_id = pkt_id, "reaction awaiting routing ack");
            }
            info!(channel, local_id, "reaction sent");
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: true,
                    error: None,
                },
            )
            .await;
        }
        Err(e) => {
            let err = format!("reaction send: {}", e);
            warn!(local_id, error = %e, "reaction send failed");
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: false,
                    error: Some(err.clone()),
                },
            )
            .await;
            send_err(event_tx, err).await;
        }
    }
}

/// Broadcast the user's geographic position on channel 0 via
/// `PortNum::PositionApp`. Latitude/longitude are converted from decimal
/// degrees (WGS84) to Meshtastic's int×1e-7 wire format. No altitude is
/// sent — we keep the wire payload small (LoRa airtime) and let peers
/// derive height from external sources if they need it.
#[instrument(
    skip(api, router, pending_acks, event_tx),
    fields(local_id, latitude, longitude)
)]
#[allow(clippy::too_many_arguments)]
async fn send_position(
    api: &mut meshtastic::api::ConnectedStreamApi<meshtastic::api::state::Configured>,
    router: &mut LocalRouter,
    pending_acks: &mut HashMap<u32, (u64, Instant)>,
    local_id: u64,
    latitude: f64,
    longitude: f64,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    // Validate before touching the radio. Out-of-range coordinates are
    // almost certainly a UI bug and would be meaningless on the wire.
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        let err = format!(
            "position out of range: lat={} lon={} (WGS84 bounds)",
            latitude, longitude
        );
        emit(
            event_tx,
            MeshEvent::SendResult {
                network: Network::Meshtastic,
                local_id,
                ok: false,
                error: Some(err.clone()),
            },
        )
        .await;
        send_err(event_tx, err).await;
        return;
    }

    // Convert degrees → int × 1e-7 (Meshtastic wire format) with rounding
    // so we don't lose precision on the last digit.
    let latitude_i = (latitude * 1e7).round() as i32;
    let longitude_i = (longitude * 1e7).round() as i32;

    let position = protobufs::Position {
        latitude_i: Some(latitude_i),
        longitude_i: Some(longitude_i),
        time: chrono::Utc::now().timestamp() as u32,
        ..Default::default()
    };
    use prost::Message;
    let mut bytes = Vec::with_capacity(position.encoded_len());
    if let Err(e) = position.encode(&mut bytes) {
        let err = format!("position encode: {}", e);
        emit(
            event_tx,
            MeshEvent::SendResult {
                network: Network::Meshtastic,
                local_id,
                ok: false,
                error: Some(err.clone()),
            },
        )
        .await;
        send_err(event_tx, err).await;
        return;
    }

    let chan = match MeshChannel::new(0) {
        Ok(c) => c,
        Err(e) => {
            let err = format!("invalid channel: {}", e);
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: false,
                    error: Some(err.clone()),
                },
            )
            .await;
            send_err(event_tx, err).await;
            return;
        }
    };

    router.last_sent_packet_id = None;
    let payload: meshtastic::types::EncodedMeshPacketData = bytes.into();
    let result = api
        .send_mesh_packet(
            router,
            payload,
            protobufs::PortNum::PositionApp,
            PacketDestination::Broadcast,
            chan,
            /* want_ack */ false, // Position is broadcast; ACKs don't really apply.
            /* want_response */ false,
            /* echo_response */ true,
            None,
            None,
        )
        .await;

    match result {
        Ok(()) => {
            if let Some(pkt_id) = router.last_sent_packet_id.take() {
                pending_acks.insert(pkt_id, (local_id, Instant::now()));
            }
            info!(local_id, latitude, longitude, "position broadcast");
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: true,
                    error: None,
                },
            )
            .await;
        }
        Err(e) => {
            let err = format!("position send: {}", e);
            warn!(local_id, error = %e, "position send failed");
            emit(
                event_tx,
                MeshEvent::SendResult {
                    network: Network::Meshtastic,
                    local_id,
                    ok: false,
                    error: Some(err.clone()),
                },
            )
            .await;
            send_err(event_tx, err).await;
        }
    }
}

#[instrument(
    skip(api, router, name, psk, event_tx),
    fields(index, role = role.as_str(), name_len = name.len(), psk_len = psk.len())
)]
async fn set_channel(
    api: &mut meshtastic::api::ConnectedStreamApi<meshtastic::api::state::Configured>,
    router: &mut LocalRouter,
    index: u32,
    role: ChannelRole,
    name: String,
    psk: Vec<u8>,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    let proto_role = match role {
        ChannelRole::Disabled => protobufs::channel::Role::Disabled,
        ChannelRole::Primary => protobufs::channel::Role::Primary,
        ChannelRole::Secondary => protobufs::channel::Role::Secondary,
    };

    let settings = if matches!(role, ChannelRole::Disabled) {
        None
    } else {
        Some(protobufs::ChannelSettings {
            psk,
            name,
            uplink_enabled: true,
            downlink_enabled: true,
            ..Default::default()
        })
    };

    let channel = protobufs::Channel {
        index: index as i32,
        role: proto_role as i32,
        settings,
    };

    info!(index, role = role.as_str(), "writing channel");
    match api.update_channel_config(router, channel).await {
        Ok(()) => info!(index, "channel write sent"),
        Err(e) => {
            warn!(index, error = %e, "channel write failed");
            send_err(event_tx, format!("set channel {}: {}", index, e)).await;
        }
    }
}

#[instrument(
    skip(api, router, event_tx, long_name, short_name),
    fields(long = long_name.len(), short = short_name.len())
)]
async fn update_user(
    api: &mut meshtastic::api::ConnectedStreamApi<meshtastic::api::state::Configured>,
    router: &mut LocalRouter,
    long_name: String,
    short_name: String,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    // The firmware expects `id` to match the node's HW id, but `update_user`
    // internally sends a SetOwner admin message; the radio ignores id for
    // the local node, so we leave it empty.
    let user = protobufs::User {
        long_name,
        short_name,
        ..Default::default()
    };

    match api.update_user(router, user).await {
        Ok(()) => info!("user update sent"),
        Err(e) => {
            warn!(error = %e, "user update failed");
            send_err(event_tx, format!("update user: {}", e)).await;
        }
    }
}

#[instrument(
    skip(api, router, cache, event_tx),
    fields(region = %region, preset = %modem_preset, hop_limit, tx_enabled, tx_power)
)]
#[allow(clippy::too_many_arguments)]
async fn set_lora_config(
    api: &mut meshtastic::api::ConnectedStreamApi<meshtastic::api::state::Configured>,
    router: &mut LocalRouter,
    cache: &ConfigCache,
    region: String,
    modem_preset: String,
    use_preset: bool,
    hop_limit: u32,
    tx_enabled: bool,
    tx_power: i32,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    // Guardrail 1: require a prior LoRa config snapshot so we never
    // blindly reset fields we don't know about (e.g. sx126x_rx_boosted_gain,
    // ignore_mqtt, paFanDisabled, …). If the radio hasn't sent its initial
    // config yet, refuse the write.
    let Some(base) = cache.lora.clone() else {
        send_err(
            event_tx,
            "radio has not yet reported its current LoRa config — wait for ConfigComplete before writing".into(),
        )
        .await;
        return;
    };

    // Guardrail 2: resolve symbolic enum names → firmware codes. Reject
    // anything we don't recognise rather than silently writing UNKNOWN(0)
    // (which maps to "UNSET"/US on some firmwares).
    let Some(region_code) =
        protobufs::config::lo_ra_config::RegionCode::from_str_name(&region)
    else {
        send_err(event_tx, format!("unknown region: {}", region)).await;
        return;
    };
    let Some(preset_code) =
        protobufs::config::lo_ra_config::ModemPreset::from_str_name(&modem_preset)
    else {
        send_err(event_tx, format!("unknown modem preset: {}", modem_preset)).await;
        return;
    };
    if hop_limit > 7 {
        send_err(
            event_tx,
            format!("hop_limit {} exceeds firmware max of 7", hop_limit),
        )
        .await;
        return;
    }
    if !(0..=30).contains(&tx_power) {
        send_err(
            event_tx,
            format!("tx_power {}dBm out of safe range (0..=30)", tx_power),
        )
        .await;
        return;
    }

    // Overlay only the user-facing fields; everything else (channel_num,
    // sx126x_rx_boosted_gain, ignore_incoming, config_ok_to_mqtt, …) keeps
    // the radio's current value.
    let lora = protobufs::config::LoRaConfig {
        region: region_code as i32,
        modem_preset: preset_code as i32,
        use_preset,
        hop_limit,
        tx_enabled,
        tx_power,
        ..base
    };
    let config = protobufs::Config {
        payload_variant: Some(protobufs::config::PayloadVariant::Lora(lora)),
    };

    info!("writing LoRa config");
    match api.update_config(router, config).await {
        Ok(()) => info!("LoRa config write sent"),
        Err(e) => {
            warn!(error = %e, "LoRa config write failed");
            send_err(event_tx, format!("LoRa config write: {}", e)).await;
        }
    }
}

#[instrument(skip(api, router, cache, event_tx), fields(role = %role))]
async fn set_device_role(
    api: &mut meshtastic::api::ConnectedStreamApi<meshtastic::api::state::Configured>,
    router: &mut LocalRouter,
    cache: &ConfigCache,
    role: String,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    let Some(base) = cache.device.clone() else {
        send_err(
            event_tx,
            "radio has not yet reported its current DeviceConfig — wait for ConfigComplete before writing"
                .into(),
        )
        .await;
        return;
    };
    let Some(role_code) = protobufs::config::device_config::Role::from_str_name(&role) else {
        send_err(event_tx, format!("unknown device role: {}", role)).await;
        return;
    };
    let device = protobufs::config::DeviceConfig {
        role: role_code as i32,
        ..base
    };
    let config = protobufs::Config {
        payload_variant: Some(protobufs::config::PayloadVariant::Device(device)),
    };

    info!("writing device role");
    match api.update_config(router, config).await {
        Ok(()) => info!("device role write sent"),
        Err(e) => {
            warn!(error = %e, "device role write failed");
            send_err(event_tx, format!("device role write: {}", e)).await;
        }
    }
}

/// Best-effort send on the event channel. If the receiver has been dropped
/// (typically because the UI is shutting down), we log at debug — there is
/// nothing actionable to do from the backend side.
async fn emit(event_tx: &mpsc::Sender<MeshEvent>, evt: MeshEvent) {
    if let Err(err) = event_tx.send(evt).await {
        debug!(error = %err, "mesh event dropped (receiver closed)");
    }
}

async fn send_err(event_tx: &mpsc::Sender<MeshEvent>, message: String) {
    emit(
        event_tx,
        MeshEvent::Error {
            network: Network::Meshtastic,
            message,
        },
    )
    .await;
}

async fn handle_packet(
    packet: protobufs::FromRadio,
    event_tx: &mpsc::Sender<MeshEvent>,
    router: &mut LocalRouter,
    pending_acks: &mut HashMap<u32, (u64, Instant)>,
    config_cache: &mut ConfigCache,
) {
    use protobufs::from_radio::PayloadVariant;

    let Some(payload) = packet.payload_variant else {
        return;
    };

    match payload {
        PayloadVariant::Packet(mesh_packet) => {
            if let Some(protobufs::mesh_packet::PayloadVariant::Decoded(data)) =
                mesh_packet.payload_variant
            {
                if data.portnum == protobufs::PortNum::RoutingApp as i32 {
                    handle_routing(&data, event_tx, pending_acks).await;
                    return;
                }
                if data.portnum == protobufs::PortNum::PositionApp as i32 {
                    handle_position(&data, mesh_packet.from, event_tx).await;
                    return;
                }
                if data.portnum == protobufs::PortNum::TelemetryApp as i32 {
                    handle_telemetry(&data, mesh_packet.from, event_tx).await;
                    return;
                }
                if data.portnum == protobufs::PortNum::TextMessageApp as i32 {
                    let text = String::from_utf8_lossy(&data.payload).to_string();
                    // Emoji reactions use the same portnum but set
                    // Data.emoji != 0 and Data.reply_id to the target
                    // packet's id. Emit as a Reaction event so the UI
                    // attaches a pill rather than creating a new bubble.
                    if data.emoji != 0 && data.reply_id != 0 {
                        debug!(
                            reply_to = data.reply_id,
                            from = mesh_packet.from,
                            emoji = %text,
                            "reaction received"
                        );
                        emit(
                            event_tx,
                            MeshEvent::Reaction {
                                network: Network::Meshtastic,
                                reply_to_packet_id: data.reply_id,
                                emoji: text,
                                from: format!("!{:08x}", mesh_packet.from),
                                timestamp: chrono::Utc::now().timestamp(),
                            },
                        )
                        .await;
                        return;
                    }
                    let rx_snr = if mesh_packet.rx_snr == 0.0 {
                        None
                    } else {
                        Some(mesh_packet.rx_snr)
                    };
                    let rx_rssi = if mesh_packet.rx_rssi == 0 {
                        None
                    } else {
                        Some(mesh_packet.rx_rssi)
                    };
                    let msg = ChatMessage {
                        timestamp: chrono::Utc::now().timestamp(),
                        network: Network::Meshtastic,
                        channel: mesh_packet.channel,
                        from: format!("!{:08x}", mesh_packet.from),
                        to: format!("!{:08x}", mesh_packet.to),
                        text,
                        local_id: None,
                        status: None,
                        rx_snr,
                        rx_rssi,
                        reply_to_text: None,
                        packet_id: if mesh_packet.id == 0 {
                            None
                        } else {
                            Some(mesh_packet.id)
                        },
                        reactions: std::collections::HashMap::new(),
                    };
                    debug!(
                        channel = msg.channel,
                        from = %msg.from,
                        bytes = msg.text.len(),
                        "text received"
                    );
                    emit(event_tx, MeshEvent::TextMessage(msg)).await;
                }
            }
        }
        PayloadVariant::NodeInfo(node) => {
            if let Some(user) = node.user {
                let (battery_level, voltage) = node
                    .device_metrics
                    .as_ref()
                    .map(|m| (m.battery_level, m.voltage))
                    .unwrap_or((None, None));
                let last_heard = if node.last_heard == 0 {
                    None
                } else {
                    Some(node.last_heard as i64)
                };
                let snr = if node.snr == 0.0 {
                    None
                } else {
                    Some(node.snr)
                };
                debug!(
                    id = %user.id,
                    long_name = %user.long_name,
                    battery = ?battery_level,
                    snr = ?snr,
                    "node_info"
                );
                emit(
                    event_tx,
                    MeshEvent::NodeSeen(NodeInfo {
                        network: Network::Meshtastic,
                        id: user.id,
                        long_name: user.long_name,
                        short_name: user.short_name,
                        battery_level,
                        voltage,
                        snr,
                        last_heard,
                        hops_away: node.hops_away,
                    }),
                )
                .await;
            }
        }
        PayloadVariant::MyInfo(my_info) => {
            router.my_id = NodeId::from(my_info.my_node_num);
            let my_id = format!("!{:08x}", my_info.my_node_num);
            info!(my_id = %my_id, "my_info received, router configured");
            emit(
                event_tx,
                MeshEvent::Connected {
                    network: Network::Meshtastic,
                    my_id,
                },
            )
            .await;
        }
        PayloadVariant::Channel(ch) => {
            if let Some(info) = convert_channel(&ch) {
                debug!(
                    index = info.index,
                    role = info.role.as_str(),
                    name = %info.name,
                    "channel received"
                );
                emit(event_tx, MeshEvent::ChannelInfo(info)).await;
            }
        }
        PayloadVariant::Config(cfg) => {
            if let Some(var) = cfg.payload_variant {
                handle_config_variant(var, event_tx, config_cache).await;
            }
        }
        PayloadVariant::ConfigCompleteId(id) => {
            info!(config_id = id, "config_complete received");
            emit(
                event_tx,
                MeshEvent::ConfigComplete {
                    network: Network::Meshtastic,
                },
            )
            .await;
        }
        _ => {}
    }
}

/// Decode a `RoutingApp` packet and, if its `request_id` matches one of
/// our outstanding sends, emit a `SendAck` event so the UI can upgrade
/// the message status. `Routing.variant = ErrorReason(0)` (NONE) is the
/// firmware's "delivered OK" signal; any other enum value is a failure.
#[instrument(skip(event_tx, pending_acks), fields(request_id = data.request_id))]
async fn handle_routing(
    data: &protobufs::Data,
    event_tx: &mpsc::Sender<MeshEvent>,
    pending_acks: &mut HashMap<u32, (u64, Instant)>,
) {
    // Drop entries we've been waiting on for too long — keeps memory
    // bounded even if the radio never echoes some acks.
    let now = Instant::now();
    pending_acks.retain(|_, (_, sent_at)| now.duration_since(*sent_at) < ACK_PENDING_TTL);

    let request_id = data.request_id;
    if request_id == 0 {
        return;
    }
    let Some((local_id, _)) = pending_acks.remove(&request_id) else {
        // Not one of ours (could be an ack for a packet from another client
        // attached to the same radio, or for a non-text portnum).
        debug!(request_id, "routing ack for untracked packet");
        return;
    };

    use prost::Message;
    let (delivered, error) = match protobufs::Routing::decode(data.payload.as_slice()) {
        Ok(routing) => match routing.variant {
            Some(protobufs::routing::Variant::ErrorReason(code)) => {
                match protobufs::routing::Error::try_from(code) {
                    Ok(protobufs::routing::Error::None) => (true, None),
                    Ok(err) => (false, Some(err.as_str_name().to_string())),
                    Err(_) => (false, Some(format!("UNKNOWN_ERROR({})", code))),
                }
            }
            // Route requests / replies carry no delivery outcome on their own;
            // treat the mere arrival (request_id matched) as a successful ack.
            Some(_) => (true, None),
            None => (true, None),
        },
        Err(e) => {
            warn!(error = %e, "Routing proto decode failed — reporting as delivered");
            (true, None)
        }
    };

    info!(
        local_id,
        request_id,
        delivered,
        err = ?error,
        "routing ack received"
    );
    emit(
        event_tx,
        MeshEvent::SendAck {
            network: Network::Meshtastic,
            local_id,
            delivered,
            error,
        },
    )
    .await;
}

/// Decode a `TelemetryApp` packet. Only `DeviceMetrics` variant is
/// surfaced — environment/air-quality/etc are left for future UIs.
#[instrument(skip(event_tx, data), fields(from))]
async fn handle_telemetry(
    data: &protobufs::Data,
    from: u32,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    use prost::Message;
    let Ok(tel) = protobufs::Telemetry::decode(data.payload.as_slice()) else {
        debug!("telemetry decode failed");
        return;
    };
    let Some(protobufs::telemetry::Variant::DeviceMetrics(m)) = tel.variant else {
        // Not a DeviceMetrics variant — drop silently. Environmental or
        // power telemetry could be handled here later.
        return;
    };
    debug!(
        from,
        battery = ?m.battery_level,
        voltage = ?m.voltage,
        ch_util = ?m.channel_utilization,
        tx_util = ?m.air_util_tx,
        uptime = ?m.uptime_seconds,
        "telemetry received"
    );
    emit(
        event_tx,
        MeshEvent::Telemetry {
            network: Network::Meshtastic,
            from: format!("!{:08x}", from),
            battery_level: m.battery_level,
            voltage: m.voltage,
            channel_utilization: m.channel_utilization,
            air_util_tx: m.air_util_tx,
            uptime_seconds: m.uptime_seconds,
            timestamp: chrono::Utc::now().timestamp(),
        },
    )
    .await;
}

/// Decode a `PositionApp` packet and emit it as a generic `Position` event.
/// Invalid / zero coordinates are dropped; a real position is never exactly
/// (0, 0) — that's the proto default when the node hasn't fixed a GPS yet.
#[instrument(skip(event_tx, data), fields(from))]
async fn handle_position(
    data: &protobufs::Data,
    from: u32,
    event_tx: &mpsc::Sender<MeshEvent>,
) {
    use prost::Message;
    let Ok(pos) = protobufs::Position::decode(data.payload.as_slice()) else {
        debug!("position decode failed");
        return;
    };
    let (lat_i, lon_i) = match (pos.latitude_i, pos.longitude_i) {
        (Some(a), Some(b)) => (a, b),
        _ => return,
    };
    // Exactly (0, 0) is the "Null Island" default for unfixed GPS; skip
    // so the UI doesn't render a peer at sea off the Gulf of Guinea.
    if lat_i == 0 && lon_i == 0 {
        return;
    }
    let latitude = f64::from(lat_i) / 1e7;
    let longitude = f64::from(lon_i) / 1e7;
    debug!(from, latitude, longitude, "position received");
    emit(
        event_tx,
        MeshEvent::Position {
            network: Network::Meshtastic,
            from: format!("!{:08x}", from),
            latitude,
            longitude,
            timestamp: chrono::Utc::now().timestamp(),
        },
    )
    .await;
}

fn convert_channel(ch: &protobufs::Channel) -> Option<ChannelInfo> {
    let role = match protobufs::channel::Role::try_from(ch.role).ok()? {
        protobufs::channel::Role::Disabled => ChannelRole::Disabled,
        protobufs::channel::Role::Primary => ChannelRole::Primary,
        protobufs::channel::Role::Secondary => ChannelRole::Secondary,
    };
    let settings = ch.settings.clone().unwrap_or_default();
    Some(ChannelInfo {
        network: Network::Meshtastic,
        index: ch.index as u32,
        role,
        name: settings.name,
        psk: settings.psk,
        uplink_enabled: settings.uplink_enabled,
        downlink_enabled: settings.downlink_enabled,
    })
}

async fn handle_config_variant(
    var: protobufs::config::PayloadVariant,
    event_tx: &mpsc::Sender<MeshEvent>,
    config_cache: &mut ConfigCache,
) {
    use protobufs::config::PayloadVariant as CPV;

    match var {
        CPV::Lora(lora) => {
            let region = protobufs::config::lo_ra_config::RegionCode::try_from(lora.region)
                .map(|r| r.as_str_name().to_string())
                .unwrap_or_else(|_| format!("UNKNOWN({})", lora.region));
            let modem_preset =
                protobufs::config::lo_ra_config::ModemPreset::try_from(lora.modem_preset)
                    .map(|p| p.as_str_name().to_string())
                    .unwrap_or_else(|_| format!("UNKNOWN({})", lora.modem_preset));
            let info = LoraInfo {
                network: Network::Meshtastic,
                region,
                modem_preset,
                use_preset: lora.use_preset,
                hop_limit: lora.hop_limit,
                bandwidth: lora.bandwidth,
                spread_factor: lora.spread_factor,
                coding_rate: lora.coding_rate,
                tx_power: lora.tx_power,
                tx_enabled: lora.tx_enabled,
            };
            debug!(
                region = %info.region,
                modem_preset = %info.modem_preset,
                "lora config received"
            );
            config_cache.lora = Some(lora);
            emit(event_tx, MeshEvent::LoraInfo(info)).await;
        }
        CPV::Device(dev) => {
            let role = protobufs::config::device_config::Role::try_from(dev.role)
                .map(|r| r.as_str_name().to_string())
                .unwrap_or_else(|_| format!("UNKNOWN({})", dev.role));
            debug!(role = %role, "device config received");
            config_cache.device = Some(dev);
            emit(
                event_tx,
                MeshEvent::DeviceRoleInfo(DeviceRoleInfo {
                    network: Network::Meshtastic,
                    role,
                }),
            )
            .await;
        }
        _ => {}
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
    use mesh_core::SendStatus;
    use prost::Message;

    /// Build a `Data` packet shaped like what the firmware emits when it
    /// acknowledges a text message: `portnum = ROUTING_APP`, `request_id`
    /// set to the acked packet's id, payload = encoded `Routing` with
    /// `ErrorReason(err)`.
    fn routing_data(request_id: u32, err: protobufs::routing::Error) -> protobufs::Data {
        let routing = protobufs::Routing {
            variant: Some(protobufs::routing::Variant::ErrorReason(err as i32)),
        };
        let mut payload = Vec::with_capacity(routing.encoded_len());
        routing.encode(&mut payload).unwrap();
        protobufs::Data {
            portnum: protobufs::PortNum::RoutingApp as i32,
            payload,
            request_id,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn routing_none_emits_delivered() {
        let (tx, mut rx) = mpsc::channel::<MeshEvent>(4);
        let mut pending: HashMap<u32, (u64, Instant)> = HashMap::new();
        pending.insert(42, (7, Instant::now()));

        handle_routing(
            &routing_data(42, protobufs::routing::Error::None),
            &tx,
            &mut pending,
        )
        .await;

        let evt = rx.recv().await.expect("event emitted");
        match evt {
            MeshEvent::SendAck {
                local_id,
                delivered,
                error,
                ..
            } => {
                assert_eq!(local_id, 7);
                assert!(delivered);
                assert!(error.is_none());
            }
            other => panic!("expected SendAck, got {:?}", other),
        }
        assert!(
            !pending.contains_key(&42),
            "entry must be removed after ack"
        );
    }

    #[tokio::test]
    async fn routing_error_emits_failed_ack() {
        let (tx, mut rx) = mpsc::channel::<MeshEvent>(4);
        let mut pending: HashMap<u32, (u64, Instant)> = HashMap::new();
        pending.insert(1234, (99, Instant::now()));

        handle_routing(
            &routing_data(1234, protobufs::routing::Error::MaxRetransmit),
            &tx,
            &mut pending,
        )
        .await;

        let evt = rx.recv().await.expect("event emitted");
        match evt {
            MeshEvent::SendAck {
                delivered, error, ..
            } => {
                assert!(!delivered);
                assert_eq!(error.as_deref(), Some("MAX_RETRANSMIT"));
            }
            other => panic!("expected SendAck, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn routing_for_untracked_packet_is_silent() {
        let (tx, mut rx) = mpsc::channel::<MeshEvent>(4);
        let mut pending: HashMap<u32, (u64, Instant)> = HashMap::new();

        handle_routing(
            &routing_data(55, protobufs::routing::Error::None),
            &tx,
            &mut pending,
        )
        .await;

        // No SendAck should have been emitted.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn routing_request_id_zero_is_ignored() {
        // `request_id == 0` means the Routing packet is a route-discovery
        // request, not an ack — never a match for an outstanding send.
        let (tx, mut rx) = mpsc::channel::<MeshEvent>(4);
        let mut pending: HashMap<u32, (u64, Instant)> = HashMap::new();
        pending.insert(0, (1, Instant::now()));

        handle_routing(
            &routing_data(0, protobufs::routing::Error::None),
            &tx,
            &mut pending,
        )
        .await;

        assert!(rx.try_recv().is_err());
        // The bogus pending_acks entry with key 0 is left untouched —
        // we short-circuit before the remove.
        assert!(pending.contains_key(&0));
    }

    #[tokio::test]
    async fn stale_pending_acks_are_evicted() {
        let (tx, _rx) = mpsc::channel::<MeshEvent>(4);
        let mut pending: HashMap<u32, (u64, Instant)> = HashMap::new();
        let long_ago = Instant::now()
            .checked_sub(ACK_PENDING_TTL + Duration::from_secs(1))
            .unwrap();
        pending.insert(111, (1, long_ago));
        pending.insert(222, (2, Instant::now()));

        // Any Routing packet triggers the GC pass. request_id 999 is not in
        // the map, so no ack is emitted — only the stale pruning matters.
        handle_routing(
            &routing_data(999, protobufs::routing::Error::None),
            &tx,
            &mut pending,
        )
        .await;

        assert!(!pending.contains_key(&111), "stale entry should be dropped");
        assert!(pending.contains_key(&222), "fresh entry should remain");
    }

    #[test]
    fn send_status_delivered_is_distinct() {
        // Defensive: the UI compares SendStatus by equality. Make sure the
        // new variant doesn't accidentally collapse with Sent.
        assert_ne!(SendStatus::Sent, SendStatus::Delivered);
    }
}
