//! Protocol-agnostic serial port discovery. Every serial-based mesh backend
//! (Meshtastic, Meshcore, ...) sees the same devices, so this helper lives
//! here rather than in a specific backend crate.

use anyhow::Result;

/// Lists available serial ports on the system.
///
/// Opportunistic filter that keeps names looking like USB-CDC devices:
/// - Linux:   `/dev/ttyACM*`, `/dev/ttyUSB*`
/// - macOS:   `/dev/cu.usbmodem*`, `/dev/cu.usbserial*`, `/dev/tty.usb*`
/// - Windows: `COM*`
///
/// If the filter matches nothing, returns the full unfiltered list as a
/// fallback so unusual setups are not hidden.
pub fn available_ports() -> Result<Vec<String>> {
    let all: Vec<String> = serialport::available_ports()?
        .into_iter()
        .map(|p| p.port_name)
        .collect();
    let filtered: Vec<String> = all
        .iter()
        .filter(|p| looks_like_usb_serial(p))
        .cloned()
        .collect();
    Ok(if filtered.is_empty() { all } else { filtered })
}

fn looks_like_usb_serial(p: &str) -> bool {
    // Linux
    if p.contains("ttyACM") || p.contains("ttyUSB") {
        return true;
    }
    // Windows
    if p.starts_with("COM") {
        return true;
    }
    // macOS — CDC-ACM shows up as /dev/cu.usbmodem* or /dev/tty.usbmodem*,
    // FTDI/CH340 as /dev/cu.usbserial*.
    if p.contains("cu.usbmodem") || p.contains("cu.usbserial") || p.contains("tty.usb") {
        return true;
    }
    false
}
