//! Diagnostic binary: connects to the radio and prints events to stdout.
//!
//! Usage:
//!     cargo run -p meshtastic-backend --bin test_connect
//!     MESH_PORT=/dev/ttyACM1 cargo run -p meshtastic-backend --bin test_connect

use mesh_core::{MeshBackend, MeshEvent};
use meshtastic_backend::MeshtasticBackend;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Same default filter as the TUI/Tauri apps: mute the upstream
    // stream_buffer ERROR-level resync spam which is benign.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,meshtastic::connections::stream_buffer=off".into()
            }),
        )
        .init();

    let port = std::env::var("MESH_PORT").unwrap_or_else(|_| "/dev/ttyACM0".to_string());
    let backend = MeshtasticBackend::new(port);
    let mut handle = backend.start().await?;

    println!("Listening... Ctrl+C to quit");
    while let Some(event) = handle.events.recv().await {
        match event {
            MeshEvent::Connected { network, my_id } => {
                println!("[CONN {}] {}", network.as_str(), my_id);
            }
            MeshEvent::Disconnected { network } => {
                println!("[DISC {}]", network.as_str());
                break;
            }
            MeshEvent::TextMessage(m) => {
                println!(
                    "[{} ch{}] {} -> {}: {}",
                    m.network.as_str(),
                    m.channel,
                    m.from,
                    m.to,
                    m.text
                );
            }
            MeshEvent::NodeSeen(n) => {
                println!(
                    "[NODE {}] {} = {} ({})",
                    n.network.as_str(),
                    n.id,
                    n.long_name,
                    n.short_name
                );
            }
            MeshEvent::Error { network, message } => {
                eprintln!("[ERR {}] {}", network.as_str(), message);
            }
            MeshEvent::ChannelInfo(c) => {
                println!(
                    "[CHAN {}] #{} role={} name={:?} psk={}",
                    c.network.as_str(),
                    c.index,
                    c.role.as_str(),
                    c.name,
                    c.psk_display()
                );
            }
            MeshEvent::LoraInfo(l) => {
                println!(
                    "[LORA {}] region={} preset={} use_preset={} hop={} tx_power={}dBm tx_enabled={}",
                    l.network.as_str(),
                    l.region,
                    l.modem_preset,
                    l.use_preset,
                    l.hop_limit,
                    l.tx_power,
                    l.tx_enabled
                );
            }
            MeshEvent::DeviceRoleInfo(d) => {
                println!("[ROLE {}] {}", d.network.as_str(), d.role);
            }
            MeshEvent::ConfigComplete { network } => {
                println!("[CFG-DONE {}]", network.as_str());
            }
            MeshEvent::SendResult {
                network,
                local_id,
                ok,
                error,
                packet_id,
            } => {
                println!(
                    "[SEND {}] id={} ok={} err={:?} packet={:?}",
                    network.as_str(),
                    local_id,
                    ok,
                    error,
                    packet_id
                );
            }
            MeshEvent::SendAck {
                network,
                local_id,
                delivered,
                error,
            } => {
                println!(
                    "[ACK {}] id={} delivered={} err={:?}",
                    network.as_str(),
                    local_id,
                    delivered,
                    error
                );
            }
            MeshEvent::Reaction {
                network,
                reply_to_packet_id,
                emoji,
                from,
                ..
            } => {
                println!(
                    "[REACT {}] {} -> packet {}: {}",
                    network.as_str(),
                    from,
                    reply_to_packet_id,
                    emoji
                );
            }
            MeshEvent::Position {
                network,
                from,
                latitude,
                longitude,
                ..
            } => {
                println!(
                    "[POS {}] {} @ {:.6}, {:.6}",
                    network.as_str(),
                    from,
                    latitude,
                    longitude
                );
            }
            MeshEvent::Telemetry {
                network,
                from,
                battery_level,
                voltage,
                channel_utilization,
                air_util_tx,
                uptime_seconds,
                ..
            } => {
                println!(
                    "[TELEM {}] {} batt={:?} V={:?} chUtil={:?} txUtil={:?} up={:?}",
                    network.as_str(),
                    from,
                    battery_level,
                    voltage,
                    channel_utilization,
                    air_util_tx,
                    uptime_seconds
                );
            }
            MeshEvent::NetworkInfo {
                wifi_enabled,
                wifi_ssid,
                eth_enabled,
                ..
            } => {
                println!(
                    "[NET] wifi={} ssid={:?} eth={}",
                    wifi_enabled, wifi_ssid, eth_enabled
                );
            }
            MeshEvent::MqttInfo {
                enabled,
                address,
                map_reporting_enabled,
                ..
            } => {
                println!(
                    "[MQTT] enabled={} addr={:?} map={}",
                    enabled, address, map_reporting_enabled
                );
            }
        }
    }
    Ok(())
}
