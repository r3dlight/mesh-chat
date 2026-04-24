//! Types and trait shared by all mesh backends (Meshtastic, Meshcore, ...).
//!
//! The UI only consumes these types. Each backend produces them from its
//! own native protocol.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Identifies the mesh network an event originated from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Network {
    Meshtastic,
    Meshcore,
}

impl Network {
    pub fn as_str(&self) -> &'static str {
        match self {
            Network::Meshtastic => "meshtastic",
            Network::Meshcore => "meshcore",
        }
    }
}

/// Status of an outgoing text message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SendStatus {
    /// Local echo, backend hasn't finished the serial write yet.
    Sending,
    /// Serial write succeeded. NOTE: this does *not* guarantee mesh delivery,
    /// only that the packet was accepted by the local radio.
    Sent,
    /// Radio confirmed end-to-end delivery via a Routing ACK.
    Delivered,
    /// Serial write failed; or routing reported a delivery error.
    Failed(String),
}

/// A text message received or sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub timestamp: i64,
    pub network: Network,
    pub channel: u32,
    pub from: String,
    pub to: String,
    pub text: String,
    /// Client-side id. `Some` only for messages we sent ourselves; used to
    /// correlate a `SendResult` event back with the original message.
    #[serde(default)]
    pub local_id: Option<u64>,
    /// Transmission status. `Some` only for our own outgoing messages.
    #[serde(default)]
    pub status: Option<SendStatus>,
    /// Signal-to-noise ratio when this message was received, in dB.
    /// `Some` only for received messages that had a non-zero measurement.
    #[serde(default)]
    pub rx_snr: Option<f32>,
    /// RSSI of the received packet, in dBm (negative number — closer to 0
    /// means stronger). `Some` only for received messages.
    #[serde(default)]
    pub rx_rssi: Option<i32>,
    /// When this message is a reply, a short quote of the parent message
    /// (typically `"@author: quoted text"`, truncated) to render as a
    /// styled block above the body. The wire payload is separately
    /// prefixed with `> ...\n` for non-mesh-chat clients, so this field
    /// is purely a display hint for our own UI.
    #[serde(default)]
    pub reply_to_text: Option<String>,
    /// Radio-level packet id as it appears on the wire. Set by backends
    /// that expose one (Meshtastic's `MeshPacket.id`); Meshcore's
    /// companion protocol doesn't give us a stable id per message, so
    /// it stays `None` there. Used to correlate incoming emoji
    /// reactions back to the message they target.
    #[serde(default)]
    pub packet_id: Option<u32>,
    /// Emoji reactions attached to this message, keyed by emoji glyph.
    /// Each value is the list of node-ids that reacted with it. Populated
    /// by the UI after a `MeshEvent::Reaction` lands.
    #[serde(default)]
    pub reactions: HashMap<String, Vec<String>>,
}

/// Information about a node on the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub network: Network,
    pub id: String,
    pub long_name: String,
    pub short_name: String,
    /// Battery percentage (0–100, or >100 if powered externally).
    #[serde(default)]
    pub battery_level: Option<u32>,
    /// Battery voltage in volts.
    #[serde(default)]
    pub voltage: Option<f32>,
    /// Signal-to-noise ratio of the last packet heard from this node, in dB.
    #[serde(default)]
    pub snr: Option<f32>,
    /// Unix timestamp when this node was last heard.
    #[serde(default)]
    pub last_heard: Option<i64>,
    /// Number of mesh hops between us and this node.
    #[serde(default)]
    pub hops_away: Option<u32>,
}

/// Role of a Meshtastic channel.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ChannelRole {
    Disabled,
    Primary,
    Secondary,
}

impl ChannelRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChannelRole::Disabled => "disabled",
            ChannelRole::Primary => "primary",
            ChannelRole::Secondary => "secondary",
        }
    }
}

/// Description of a single channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub network: Network,
    pub index: u32,
    pub role: ChannelRole,
    pub name: String,
    /// Raw PSK: 0 bytes = no crypto; 1 byte = "default N" shorthand
    /// (Meshtastic convention); 16/32 bytes = AES128/256.
    pub psk: Vec<u8>,
    pub uplink_enabled: bool,
    pub downlink_enabled: bool,
}

impl ChannelInfo {
    /// Human-readable PSK summary.
    pub fn psk_display(&self) -> String {
        match self.psk.len() {
            0 => "none".to_string(),
            1 => format!("default{}", self.psk[0]),
            16 => "AES128 (custom)".to_string(),
            32 => "AES256 (custom)".to_string(),
            n => format!("{} bytes (invalid)", n),
        }
    }

    /// Privacy tier of the channel based on the PSK encoding.
    ///
    /// `Private` only if the PSK is a 16- or 32-byte custom key known solely
    /// to the participants. A 1-byte "defaultN" PSK encrypts on the wire but
    /// with a key hardcoded in the firmware (and therefore public knowledge),
    /// so it counts as `Public` from a privacy standpoint.
    pub fn privacy(&self) -> ChannelPrivacy {
        match self.psk.len() {
            16 | 32 => ChannelPrivacy::Private,
            _ => ChannelPrivacy::Public,
        }
    }
}

/// How safe messages on this channel are from being read by strangers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelPrivacy {
    /// No PSK, or a well-known "defaultN" shorthand. Messages are trivially
    /// readable by any other Meshtastic user.
    Public,
    /// Custom AES-128 or AES-256 PSK known only to participants.
    Private,
}

impl ChannelPrivacy {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChannelPrivacy::Public => "public",
            ChannelPrivacy::Private => "private",
        }
    }
}

/// LoRa radio parameters (region, preset, power, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoraInfo {
    pub network: Network,
    pub region: String,
    pub modem_preset: String,
    pub use_preset: bool,
    pub hop_limit: u32,
    pub bandwidth: u32,
    pub spread_factor: u32,
    pub coding_rate: u32,
    pub tx_power: i32,
    pub tx_enabled: bool,
}

/// Device role (CLIENT, ROUTER, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRoleInfo {
    pub network: Network,
    pub role: String,
}

/// Event emitted by a backend towards the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MeshEvent {
    Connected {
        network: Network,
        my_id: String,
    },
    Disconnected {
        network: Network,
    },
    TextMessage(ChatMessage),
    NodeSeen(NodeInfo),
    ChannelInfo(ChannelInfo),
    LoraInfo(LoraInfo),
    DeviceRoleInfo(DeviceRoleInfo),
    /// Emitted once the radio has finished dumping its initial config.
    ConfigComplete {
        network: Network,
    },
    /// Outcome of a `MeshCommand::SendText` attempt. Correlate with the
    /// original message via `local_id`.
    SendResult {
        network: Network,
        local_id: u64,
        ok: bool,
        error: Option<String>,
        /// Radio-level packet id of the dispatched message, when the
        /// backend can surface it (Meshtastic captures it via
        /// `PacketRouter::handle_mesh_packet`; Meshcore's companion
        /// protocol has no equivalent so it stays `None`). The UI uses
        /// this to match incoming emoji reactions back to the message
        /// the user just sent.
        #[serde(default)]
        packet_id: Option<u32>,
    },
    /// Radio-layer acknowledgement of an earlier send. `delivered = true`
    /// means the firmware received a Routing ACK from the mesh; otherwise
    /// `error` describes why routing failed (e.g. `MAX_RETRANSMIT`,
    /// `NO_CHANNEL`). Correlate with the original send via `local_id`.
    SendAck {
        network: Network,
        local_id: u64,
        delivered: bool,
        error: Option<String>,
    },
    /// Incoming emoji reaction to a prior message. Backends forward it
    /// with the radio-level `reply_to_packet_id` of the target message;
    /// the UI is responsible for looking up the matching `ChatMessage`
    /// (via its `packet_id`) and attaching the reaction.
    Reaction {
        network: Network,
        reply_to_packet_id: u32,
        emoji: String,
        from: String,
        timestamp: i64,
    },
    /// A peer shared their geographic position. Rendered as a pill on the
    /// node's most recent bubble (or in the sidebar if we haven't received
    /// any chat from them yet). Coordinates are decimal degrees WGS84.
    Position {
        network: Network,
        from: String,
        latitude: f64,
        longitude: f64,
        timestamp: i64,
    },
    /// Current network (WiFi / Ethernet) config as reported by the
    /// radio. Sensitive fields (PSK) are never echoed — the firmware
    /// doesn't return them either, so the UI treats WiFi password as
    /// a write-only field.
    NetworkInfo {
        network: Network,
        wifi_enabled: bool,
        wifi_ssid: String,
        eth_enabled: bool,
    },
    /// Current MQTT module config. Password is intentionally not
    /// carried for the same reason as `NetworkInfo.wifi_psk`.
    MqttInfo {
        network: Network,
        enabled: bool,
        address: String,
        username: String,
        encryption_enabled: bool,
        tls_enabled: bool,
        map_reporting_enabled: bool,
        root: String,
    },
    /// Device telemetry snapshot: battery, voltage, channel utilization,
    /// airtime TX %, uptime. Broadcast by each node at a configurable
    /// cadence (default 30min on Meshtastic). All metric fields are
    /// optional because the firmware may not populate them.
    Telemetry {
        network: Network,
        from: String,
        /// Battery percentage 0–100, or >100 if externally powered.
        battery_level: Option<u32>,
        /// Battery voltage in volts.
        voltage: Option<f32>,
        /// Utilization of the current channel, 0–100%.
        channel_utilization: Option<f32>,
        /// TX airtime used in the last hour, 0–100%.
        air_util_tx: Option<f32>,
        /// Seconds since last reboot.
        uptime_seconds: Option<u32>,
        timestamp: i64,
    },
    Error {
        network: Network,
        message: String,
    },
}

/// Command sent by the UI to a backend.
#[derive(Debug, Clone)]
pub enum MeshCommand {
    SendText {
        /// Client-side id echoed back in `MeshEvent::SendResult` for matching.
        local_id: u64,
        channel: u32,
        text: String,
        /// If `Some`, a direct message addressed to the given node id
        /// (e.g. `"!49b5b33c"`). If `None`, broadcast on the channel.
        to: Option<String>,
    },
    /// Upsert a channel at a given index. Setting `role` to `Disabled` wipes
    /// the slot (i.e. deletes the channel). The radio echoes back the new
    /// state as a `ChannelInfo` event once the write lands.
    SetChannel {
        index: u32,
        role: ChannelRole,
        name: String,
        /// Raw PSK bytes. See `ChannelInfo::psk` for the encoding convention
        /// (0 = none, 1 byte = "defaultN" shorthand, 16/32 = AES128/256).
        psk: Vec<u8>,
    },
    /// Update the local node's public identity (long + short name). The new
    /// name is broadcast to the mesh via periodic NodeInfo packets.
    UpdateUser {
        long_name: String,
        short_name: String,
    },
    /// Write a new LoRa radio config to the local node. The backend converts
    /// the symbolic enum names (e.g. `"EU_868"`, `"LONG_FAST"`) to the
    /// firmware's integer codes and rejects unknown values. Changing region
    /// or modem preset triggers a reboot of the radio.
    SetLoraConfig {
        region: String,
        modem_preset: String,
        use_preset: bool,
        hop_limit: u32,
        tx_enabled: bool,
        tx_power: i32,
    },
    /// Write a new device role (`"CLIENT"`, `"CLIENT_MUTE"`, `"ROUTER"`, …).
    /// Wrong role (e.g. ROUTER on a battery node) drains battery or
    /// monopolises airtime — callers must guardrail before sending.
    SetDeviceRole {
        role: String,
    },
    /// Write WiFi credentials. Empty `wifi_psk` is treated as an open
    /// network by the firmware. Switching `wifi_enabled = true` also
    /// disables Bluetooth on most ESP32 builds (shared radio chain).
    SetNetworkConfig {
        wifi_enabled: bool,
        wifi_ssid: String,
        wifi_psk: String,
    },
    /// Ask the backend to re-emit its known-nodes list so the UI can
    /// recover from stale cached entries (e.g. a remote node that got
    /// renamed but whose old name is still in the firmware's contact
    /// cache). Each backend does what it can:
    /// - Meshcore: re-runs `get_contacts(0)` and emits a NodeSeen per
    ///   contact.
    /// - Meshtastic: no "query all nodes" primitive; emits an Error
    ///   event explaining the limitation.
    RefreshNodes,
    /// Write MQTT module config. Setting `map_reporting_enabled = true`
    /// with `enabled = true` plus WiFi up is what gets the node on
    /// meshmap.net / meshtastic.org/map.
    SetMqttConfig {
        enabled: bool,
        address: String,
        username: String,
        password: String,
        encryption_enabled: bool,
        tls_enabled: bool,
        map_reporting_enabled: bool,
        root: String,
    },
    /// Send an emoji reaction to a previously-received packet. Only
    /// Meshtastic supports this natively (`Data.emoji=1 + Data.reply_id`);
    /// the Meshcore backend rejects it with an `Error` event so the UI
    /// can show a tooltip.
    SendReaction {
        local_id: u64,
        channel: u32,
        /// DM target (12/8-char node id) or `None` for channel broadcast.
        to: Option<String>,
        reply_to_packet_id: u32,
        emoji: String,
    },
    /// Broadcast the user's current geographic position. Latitude and
    /// longitude are in decimal degrees (WGS84). Each backend converts to
    /// its native unit — Meshtastic uses int × 1e-7 degrees on the wire,
    /// Meshcore's companion API takes degrees as `f64` directly.
    SendPosition {
        local_id: u64,
        latitude: f64,
        longitude: f64,
    },
    Shutdown,
}

/// Communication handle into a running backend.
pub struct BackendHandle {
    pub events: mpsc::Receiver<MeshEvent>,
    pub commands: mpsc::Sender<MeshCommand>,
}

/// Abstraction over a mesh backend. Each network (Meshtastic, Meshcore, ...)
/// provides its own implementation of this trait.
#[async_trait]
pub trait MeshBackend: Send + Sync {
    /// The network this backend covers.
    fn network(&self) -> Network;

    /// Starts the backend's internal loop and returns the events/commands
    /// channels. The background task runs until a `MeshCommand::Shutdown`
    /// is received or the physical transport disconnects.
    async fn start(&self) -> anyhow::Result<BackendHandle>;
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

    fn chan(psk: Vec<u8>) -> ChannelInfo {
        ChannelInfo {
            network: Network::Meshtastic,
            index: 0,
            role: ChannelRole::Secondary,
            name: "test".into(),
            psk,
            uplink_enabled: true,
            downlink_enabled: true,
        }
    }

    #[test]
    fn network_as_str() {
        assert_eq!(Network::Meshtastic.as_str(), "meshtastic");
        assert_eq!(Network::Meshcore.as_str(), "meshcore");
    }

    #[test]
    fn channel_role_as_str() {
        assert_eq!(ChannelRole::Disabled.as_str(), "disabled");
        assert_eq!(ChannelRole::Primary.as_str(), "primary");
        assert_eq!(ChannelRole::Secondary.as_str(), "secondary");
    }

    #[test]
    fn psk_display_by_length() {
        assert_eq!(chan(vec![]).psk_display(), "none");
        assert_eq!(chan(vec![1]).psk_display(), "default1");
        assert_eq!(chan(vec![7]).psk_display(), "default7");
        assert_eq!(chan(vec![0u8; 16]).psk_display(), "AES128 (custom)");
        assert_eq!(chan(vec![0u8; 32]).psk_display(), "AES256 (custom)");
        assert_eq!(chan(vec![0u8; 5]).psk_display(), "5 bytes (invalid)");
    }

    #[test]
    fn privacy_classification() {
        // Only 16/32-byte custom keys count as private — default shortcuts and
        // empty PSK are both public from a "who can read" standpoint.
        assert_eq!(chan(vec![]).privacy(), ChannelPrivacy::Public);
        assert_eq!(chan(vec![1]).privacy(), ChannelPrivacy::Public);
        assert_eq!(chan(vec![10]).privacy(), ChannelPrivacy::Public);
        assert_eq!(chan(vec![0u8; 7]).privacy(), ChannelPrivacy::Public);
        assert_eq!(chan(vec![0u8; 16]).privacy(), ChannelPrivacy::Private);
        assert_eq!(chan(vec![0u8; 32]).privacy(), ChannelPrivacy::Private);
    }

    #[test]
    fn chat_message_roundtrip_with_optionals() {
        let msg = ChatMessage {
            timestamp: 1234567890,
            network: Network::Meshtastic,
            channel: 1,
            from: "!abcd1234".into(),
            to: "^all".into(),
            text: "hello".into(),
            local_id: Some(42),
            status: Some(SendStatus::Sent),
            rx_snr: Some(-3.5),
            rx_rssi: Some(-84),
            reply_to_text: None,
            packet_id: None,
            reactions: HashMap::new(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ChatMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.local_id, Some(42));
        assert!(matches!(back.status, Some(SendStatus::Sent)));
        assert_eq!(back.rx_rssi, Some(-84));
        assert_eq!(back.text, "hello");
    }

    #[test]
    fn chat_message_deserialize_legacy_without_new_fields() {
        // Old plaintext history lines only had the pre-optional fields; we
        // must still be able to read them without errors.
        let legacy = r#"{
            "timestamp": 1,
            "network": "Meshtastic",
            "channel": 0,
            "from": "!abcd",
            "to": "^all",
            "text": "hi"
        }"#;
        let msg: ChatMessage = serde_json::from_str(legacy).expect("parse legacy");
        assert_eq!(msg.text, "hi");
        assert!(msg.local_id.is_none());
        assert!(msg.status.is_none());
        assert!(msg.rx_snr.is_none());
        assert!(msg.rx_rssi.is_none());
        assert!(msg.reply_to_text.is_none());
        assert!(msg.packet_id.is_none());
        assert!(msg.reactions.is_empty());
    }

    #[test]
    fn chat_message_with_reply_roundtrips() {
        let msg = ChatMessage {
            timestamp: 1,
            network: Network::Meshtastic,
            channel: 0,
            from: "!a".into(),
            to: "^all".into(),
            text: "sure, agree".into(),
            local_id: None,
            status: None,
            rx_snr: None,
            rx_rssi: None,
            reply_to_text: Some("@bob: should we do X?".into()),
            packet_id: None,
            reactions: HashMap::new(),
        };
        let js = serde_json::to_string(&msg).expect("serialize");
        let back: ChatMessage = serde_json::from_str(&js).expect("deserialize");
        assert_eq!(
            back.reply_to_text.as_deref(),
            Some("@bob: should we do X?")
        );
    }

    #[test]
    fn chat_message_with_reactions_roundtrips() {
        let mut reactions = HashMap::new();
        reactions.insert("👍".to_string(), vec!["!alice".to_string(), "!bob".to_string()]);
        reactions.insert("❤".to_string(), vec!["!carol".to_string()]);
        let msg = ChatMessage {
            timestamp: 1,
            network: Network::Meshtastic,
            channel: 0,
            from: "!me".into(),
            to: "^all".into(),
            text: "great work".into(),
            local_id: None,
            status: None,
            rx_snr: None,
            rx_rssi: None,
            reply_to_text: None,
            packet_id: Some(0xdeadbeef),
            reactions,
        };
        let js = serde_json::to_string(&msg).expect("serialize");
        let back: ChatMessage = serde_json::from_str(&js).expect("deserialize");
        assert_eq!(back.packet_id, Some(0xdeadbeef));
        assert_eq!(back.reactions.get("👍").map(Vec::len), Some(2));
        assert_eq!(back.reactions.get("❤").map(Vec::len), Some(1));
    }

    #[test]
    fn channel_privacy_as_str() {
        assert_eq!(ChannelPrivacy::Public.as_str(), "public");
        assert_eq!(ChannelPrivacy::Private.as_str(), "private");
    }

    #[test]
    fn send_status_delivered_roundtrip() {
        // Delivered is the new variant — ensure it serialises and deserialises
        // cleanly so the history file can contain it.
        let js = serde_json::to_string(&SendStatus::Delivered).expect("serialize");
        let back: SendStatus = serde_json::from_str(&js).expect("deserialize");
        assert_eq!(back, SendStatus::Delivered);
    }

    #[test]
    fn send_ack_event_roundtrip() {
        let evt = MeshEvent::SendAck {
            network: Network::Meshtastic,
            local_id: 7,
            delivered: true,
            error: None,
        };
        let js = serde_json::to_string(&evt).expect("serialize");
        let back: MeshEvent = serde_json::from_str(&js).expect("deserialize");
        match back {
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
            other => panic!("unexpected variant: {:?}", other),
        }
    }
}
