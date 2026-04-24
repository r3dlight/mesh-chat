<p align="center">
  <img src="ui/public/logo.svg" alt="mesh-chat logo" width="140" />
</p>

<h1 align="center">mesh-chat</h1>

<p align="center">
  <em>A chat client for LoRa mesh networks — Rust, TUI + desktop GUI.</em>
</p>

---

> ⚠ **Tested only on Linux (Debian/Ubuntu) with a Heltec WiFi LoRa 32 V4
> running Meshtastic firmware 2.7.15.** Windows and macOS builds are
> produced by CI but **have not been verified on hardware**. Other
> Meshtastic-compatible boards should work over USB-CDC but are untested.

A chat client for LoRa mesh networks, written in Rust. Two frontends
share the same Rust backend:

- **`mesh-chat-tui`** — ratatui terminal client, feature-complete
- **`mesh-chat-desktop`** — Tauri 2 desktop app (Vue 3 / Vite UI)

Protocol logic lives behind a `MeshBackend` trait; both frontends only
consume normalized events. A second backend (Meshcore) can be plugged in
without touching UI code.

## Features

**Chat experience**

- Channels and DMs share the **same sidebar** — Tab / click to cycle
  through all of them. No separate "DM mode".
- Chat bubbles with left / right alignment, color-coded (green = you,
  cyan = them), `me (long_name)` label on own messages.
- Per-message **RSSI / SNR** on received packets, send-status glyph
  (`…` / `✓` / `✗`) on outgoing ones.
- Unread badges (`+N`) on every inactive channel and DM thread.
- Date separators in long conversations, scroll with PgUp/PgDn (TUI) or
  natural scrolling (Tauri), scrollbar widget with overflow indicator.

**Privacy indicators — impossible to miss**

- **PRIVATE** badge (green) only for channels with a custom 16- or
  32-byte PSK, or for DMs (firmware PKC end-to-end).
- **PUBLIC** badge (red) otherwise — including `default*` keys, which
  are hardcoded in every Meshtastic firmware and therefore public.
- Header badge, chat pane title, input border: all reflect the current
  space's privacy state.

**Channel management (TUI + Tauri)**

- List, create, edit, delete channels. Primary (#0) is read-only.
- PSK presets: `default`, `default2`..`default10`, `none`, `random16`
  (AES-128 CSPRNG), `random32` (AES-256 CSPRNG).
- Optimistic local update + `y`/click confirmation before any write.

**Nodes & direct messages**

- Nodes view: id, long_name, battery, SNR, hops, last-heard (relative),
  position pin (if known), per-user **alias** and **favorite star**.
- **Start DM** from nodes list: opens or reuses a thread with that peer
  and switches the current space to it. DMs are end-to-end encrypted
  when both peers have PKC keys (Meshtastic 2.5+).
- Favorited DMs float to the top of the sidebar. Aliases and favorites
  are persisted atomically in `$XDG_DATA_HOME/mesh-chat/aliases.json`.

**Dual-protocol backends**

- **Meshtastic** backend via the upstream `meshtastic` crate (protobuf
  over USB-CDC). Full feature set.
- **Meshcore** backend via `meshcore-rs` (Ripple companion-radio protocol
  on serial). The companion protocol has no native reaction or
  packet-id primitive, so a handful of Meshtastic-specific features
  (✓✓ delivery ack, emoji reactions) are gated off when on Meshcore —
  the UI shows an explicit tooltip.
- Select via `[general] network = "meshtastic" | "meshcore"` in
  `config.toml`. Defaults to `meshtastic` for backwards compat.

**Per-message actions**

- **Reply** (↩): inline composer bar with the quoted preview. The wire
  payload is prefixed `> @author: quote\n…` so non-mesh-chat clients
  still see the reference.
- **Forward** (↗): pick any other space from a modal; sends the text
  and switches to the destination.
- **Emoji reactions** (Meshtastic only): 8 inline emojis under every
  received bubble, one click to send. Uses the native `emoji=1` proto
  flag so the Meshtastic mobile apps render them as pills too, not
  bubbles. Pills on the bubble aggregate reactions by sender.
- **End-to-end delivery ack** (`✓✓`): captured by matching the radio's
  Routing packets back to outgoing message ids. Routing failures
  (`MAX_RETRANSMIT`, `NO_CHANNEL`, …) are surfaced as `✗` with the
  firmware error code as tooltip.

**Search, positions, telemetry**

- **In-space search** (Ctrl+F): case-insensitive filter on message
  bodies and reply quotes, live match count.
- **Position sharing** (📍): broadcasts your GPS via `PortNum::PositionApp`
  (Meshtastic) or `set_coords` (Meshcore). Received positions render as
  a pill under the sender's bubbles with an OpenStreetMap link, plus a
  column in the Nodes modal.
- **Radio telemetry** (📊): periodic `DeviceMetrics` packets decoded
  into a panel — battery, voltage, channel utilisation, TX airtime %,
  uptime per node.

**Desktop notifications**

- Tauri notifications fire on inactive-window DMs and on channel
  messages that mention your long_name (case-insensitive substring).
  Opt-in at the OS level on first use.

**Channel sharing**

- Every secondary channel has a **Share** button that generates a
  `meshtastic.org/e/#<base64>` URL (standard Meshtastic channel-set
  encoding) plus a QR code SVG. Any Meshtastic app imports it by
  scanning.

**Radio configuration**

- **Read**: LoRa region, modem preset, hop limit, bandwidth, SF, coding
  rate, TX power, device role.
- **Write** (Tauri, `⚙ radio` button / `r` shortcut): region, modem
  preset, device role, hop limit (0–7), TX enabled, TX power (0–30 dBm).
  The backend overlays only user-edited fields on top of the last
  config snapshot from the radio, so untouched fields (e.g.
  `sx126x_rx_boosted_gain`) keep their current value. Two-step confirm
  with a diff preview before the write hits the wire.
- Node identity (`long_name` / `short_name`) is writable from the
  `👤` toolbar button in Tauri or `e` in the Settings modal of the TUI.

**Device discovery**

- Scans `/dev/ttyACM*`, `/dev/ttyUSB*` (Linux), `/dev/cu.usbmodem*`,
  `/dev/cu.usbserial*`, `/dev/tty.usb*` (macOS), `COM*` (Windows).
- Overrides: CLI `--port X`, env `MESH_PORT`, config `general.port`.

**Storage**

- Append-only history at `$XDG_DATA_HOME/mesh-chat/history.jsonl`.
- **Optional encryption** via passphrase + Argon2id → ChaCha20-Poly1305.
  No key material on disk — passphrase asked at every launch. See
  [Encryption](#encryption-passphrase-based) below.
- Config at `$XDG_CONFIG_HOME/mesh-chat/config.toml`.
- `--dump-history` flag (TUI) decrypts and prints to stdout.

**Security posture**

- `#![forbid(unsafe_code)]` workspace-wide.
- ANSSI-style clippy lints (`unwrap_used`, `expect_used`, `panic`,
  `todo`, `unimplemented` — all warned).
- `overflow-checks = true` in release builds.
- Argon2id `m=65536, t=3, p=4` + 16-byte random salt per file.

## Installation

> Windows and macOS builds are produced by CI but have **not been
> verified on hardware**. Reports welcome.

### From a release (prebuilt)

Grab the asset matching your platform from the
[latest release](../../releases/latest).

**Linux — AppImage** (portable, no install):

```bash
chmod +x mesh-chat-desktop_*.AppImage
./mesh-chat-desktop_*.AppImage
```

**Linux — `.deb`** (Ubuntu / Debian):

```bash
sudo dpkg -i mesh-chat-desktop_*.deb
sudo apt --fix-broken install     # pulls missing runtime deps if needed
```

On Linux, add yourself to `dialout` once so the app can open the serial
port without `sudo`:

```bash
sudo usermod -a -G dialout $USER
# log out / log in (or reboot) for the group to take effect
```

**Windows — `.msi` installer** or **`.exe` portable**:

- Double-click the `.msi` and follow the installer; the app appears in
  the Start menu.
- The `.exe` is a standalone binary — run it from any folder.
- Windows Defender may prompt on first run: the bundles are not
  code-signed yet (TODO).

**macOS — `.dmg`**:

1. Double-click the `.dmg` to mount it.
2. Drag `mesh-chat.app` into `/Applications`.
3. First launch: right-click the app → **Open** (bypasses Gatekeeper
   since bundles aren't notarised yet).

### From source

Clone + build. Requirements below are per-OS.

**Linux (Ubuntu / Debian)**:

```bash
sudo apt install build-essential pkg-config libssl-dev libudev-dev
# GUI only — adds WebKitGTK + friends:
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
                 librsvg2-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev
sudo usermod -a -G dialout $USER    # log out/in afterwards

git clone <repo-url> mesh-chat && cd mesh-chat
cargo run -p mesh-chat-tui                  # TUI
cd ui && npm install && cd ../src-tauri
cargo tauri dev                             # GUI (hot reload)
```

**Windows 11** (PowerShell as Administrator for the toolchain
install — not for the app itself):

```powershell
# 1. Rust toolchain (https://rustup.rs — installs MSVC prerequisites automatically).
# 2. Node.js LTS from https://nodejs.org (≥ 22).
# 3. WebView2 runtime (bundled with Win 11; separate installer on Win 10).
# 4. Optional: winget install --id Microsoft.VisualStudio.2022.BuildTools

git clone <repo-url> mesh-chat
cd mesh-chat
cargo run -p mesh-chat-tui                  # TUI
cd ui && npm install && cd ..\src-tauri
cargo tauri dev                             # GUI
```

**macOS** (11+ with Xcode Command Line Tools):

```bash
xcode-select --install                      # one-time
brew install node                           # or install from nodejs.org
# Rust toolchain: https://rustup.rs

git clone <repo-url> mesh-chat && cd mesh-chat
cargo run -p mesh-chat-tui                  # TUI
cd ui && npm install && cd ../src-tauri
cargo tauri dev                             # GUI
```

Plus [Rust](https://rustup.rs) on every platform, and Node.js ≥ 22 for
the desktop build.

## Running

**TUI**

```bash
cargo run -p mesh-chat-tui                           # scan + pick
cargo run -p mesh-chat-tui -- --port /dev/ttyACM0    # explicit
MESH_PORT=/dev/ttyACM0 cargo run -p mesh-chat-tui    # env
cargo run -p mesh-chat-tui -- --dump-history         # decrypt to stdout
```

**Desktop**

```bash
cd ui && npm install && cd ..
cd src-tauri && cargo tauri dev                       # hot-reload dev
cd src-tauri && cargo tauri build --bundles appimage  # release bundle
```

**Diagnostic binary** (no UI, prints every mesh event to stdout):

```bash
cargo run -p meshtastic-backend --bin test_connect
```

## TUI keybindings

| Key                 | Action                                     |
| ------------------- | ------------------------------------------ |
| `Tab` / `Shift-Tab` | Next / previous space (channel or DM)      |
| `Enter`             | Send message                               |
| `PgUp` / `PgDn`     | Scroll messages                            |
| `s`                 | Settings modal                             |
| `c`                 | Channels modal (CRUD)                      |
| `n`                 | Nodes modal                                |
| `d`                 | Jump to most recent DM                     |
| `Esc`               | Close modal / quit when on Main            |
| `Ctrl+C`            | Quit from anywhere                         |

`s`, `c`, `n`, `d` only open when the input is empty (otherwise they
are typed into the current message).

**Channels modal** — `↑↓` select · `n` new · `e` edit · `d` delete
(primary is read-only).

**Channel editor** — `Tab` switches Name / PSK field · `← →` cycles
PSK presets · `Enter` validates → diff preview → `y` confirms.

**Nodes modal** — `↑↓` select · `Enter` opens a DM with the selected
node.

**Settings modal** — `e` edits `long_name` / `short_name`.

## Tauri desktop

Open the app, enter your history passphrase if encryption is on, then:

- **Sidebar toolbar buttons**: `👤 me` (identity), `# chans` (channel
  CRUD), `⧉ nodes` (mesh roster with Start DM).
- **Sidebar list**: all channels + DM threads, click to switch.
- **Header chips**: history encryption state, channel privacy, live
  connection status (pulsing green = OK).

### Starting a DM

1. Click `⧉ nodes` in the sidebar.
2. Click the peer row you want to message.
3. Click `✉ Start DM`.

The modal closes, a new thread appears in the sidebar, and the current
space switches to it. Reply with Enter; incoming DMs land in the
thread automatically (unread badge if you're not viewing it).

### What makes a DM different from a channel?

A **channel** broadcasts to every node sharing the same PSK. A **DM**
targets one specific node by hex id (`!49b5b33c`). Meshtastic 2.5+
firmware encrypts the payload end-to-end using the recipient's
Curve25519 public key (PKC) — only the recipient can decrypt it, so
relaying nodes see only ciphertext. DMs get the green `PRIVATE` badge
even when they ride on a `PUBLIC` channel.

## Configuration

`$XDG_CONFIG_HOME/mesh-chat/config.toml` (typically
`~/.config/mesh-chat/config.toml`). All fields are optional.

```toml
[general]
port = "/dev/ttyACM0"
log_dir = "/var/log/mesh-chat"   # TUI tracing logs

[history]
encrypt = false                   # enable passphrase-based encryption
```

Port resolution precedence: `--port X` → `MESH_PORT` → `general.port`
→ single `/dev/tty…` detected → interactive picker.

## History

`$XDG_DATA_HOME/mesh-chat/history.jsonl` (one JSON message per line,
append-only). Replayed at startup in both frontends. Delete the file to
reset.

### Encryption (passphrase-based)

```bash
mkdir -p ~/.config/mesh-chat
printf '[history]\nencrypt = true\n' >> ~/.config/mesh-chat/config.toml
# If there's an existing plaintext history, move it aside first:
mv ~/.local/share/mesh-chat/history.jsonl{,.bak} 2>/dev/null || true
```

Then launch the TUI or the desktop app. **First launch** asks you to
set a passphrase (twice). Subsequent launches ask you to unlock.

**Format (v2)**: first line is

```
#MESHCHAT-ENC-V2 argon2id m=65536 t=3 p=4 salt=<base64-16>
```

second line is a **canary** — the fixed plaintext `MESHCHAT-CANARY-V2`
encrypted with the derived key. Subsequent lines are one
`base64(nonce_12 || ciphertext || tag_16)` per chat message.
Key = Argon2id(passphrase, salt) → 32 bytes. Held in memory only.

The canary exists so that `unlock` can detect a wrong passphrase even
on a freshly-created history that has no real messages yet. Without it,
a wipe followed by a typo would silently accept the bad passphrase
(nothing to decrypt to verify against) and write subsequent messages
under a key the user could no longer reproduce — effectively bricking
the store. Files created before the canary shipped are still readable:
`unlock` falls back to verifying against the first real encrypted
message, same as before.

If a file somehow ends up with mismatched salt / key (e.g. from older
app versions or manual tampering), the only recovery is to move the
file aside and start fresh:

```bash
mv ~/.local/share/mesh-chat/history.jsonl{,.corrupt}
```

**Frontends indicate state** with a chip at the top:

- `📄 plaintext` — encryption disabled in config
- `🔒 locked` — encryption enabled, passphrase modal showing
- `🔒 history` — unlocked and writing live

Dump the decrypted history to stdout (TUI):

```bash
mesh-chat-tui --dump-history > backup.jsonl
```

## Architecture

```
mesh-chat/
├── Cargo.toml                  # workspace
├── mesh-core/                  # MeshBackend trait + shared types + tests
├── mesh-lib/                   # shared utilities: history, config, serial
├── meshtastic-backend/         # Meshtastic impl (serial + protobuf)
│   └── src/bin/test_connect.rs
├── tui/                        # ratatui client
├── src-tauri/                  # Tauri 2 shell (Rust side)
│   ├── capabilities/           # ACL permissions (event/app/window/webview)
│   └── icons/
└── ui/                         # Vue 3 + Vite frontend
    ├── public/                 # static assets (logo.svg) — never `import`
    └── src/App.vue
```

### Backend trait

```rust
#[async_trait]
pub trait MeshBackend: Send + Sync {
    fn network(&self) -> Network;
    async fn start(&self) -> Result<BackendHandle>;
}
```

`BackendHandle` = `Receiver<MeshEvent>` (messages, nodes, channels,
LoRa config, send results, errors) + `Sender<MeshCommand>` (SendText,
SetChannel, UpdateUser, Shutdown). The Tauri app runs the backend on a
**dedicated OS thread with its own tokio runtime**, isolated from
Tauri's main runtime to prevent event-loop starvation from fragmenting
serial reads.

## Tests and CI

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
```

GitHub Actions (`.github/workflows/ci.yml`):

- **lint**: clippy on Linux (rustfmt check disabled — it fights with
  hand-wrapped multi-line expressions in test helpers).
- **test**: build + `cargo test` on ubuntu / windows / macos (Tauri
  crate excluded on non-Linux).
- **bundle**: produces `.AppImage + .deb` on Linux, `.msi + .exe` on
  Windows, `.app + .dmg` on macOS — uploaded as workflow artifacts.
- **release** (tag pushes only): downloads bundles from the matrix
  and publishes them as assets on a GitHub Release. Pre-1.0 tags
  (`v0.*`) are marked as prereleases. Cut a release by pushing a tag:
  ```bash
  git tag v0.1.0
  git push origin v0.1.0
  ```

## Gotchas learned the hard way

- **`Broken pipe` on serial open**: the ESP32 auto-reset triggered by
  DTR causes the USB-CDC to disconnect mid-configure. The backend
  retries 5× with growing backoff and switches to `DTR=false, RTS=false`
  after 2 failed resets. If it still fails, unplug / replug the radio.
- **`'image/svg+xml' is not a valid JavaScript MIME type`** in the
  Tauri webview: WebKit rejects SVG imports via the ES module loader.
  All static assets go under `ui/public/` and are referenced with
  plain string URLs — never `import logo from "./logo.svg"`. See
  `ui/public/README.md`.
- **Tauri v2 capabilities**: events only flow to the webview if the
  capability in `src-tauri/capabilities/default.json` grants
  `core:event:default`. Missing = silent blocking.
- **localhost vs 127.0.0.1**: Vite dev server binds explicitly to
  `127.0.0.1`, so `devUrl` in `tauri.conf.json` must match (IPv6
  `::1` resolution of `localhost` on recent Linux breaks the webview).

## TODO

- [ ] Real app icons (current set is a generated placeholder)
- [ ] Verify Windows and macOS builds on actual hardware (CI currently
      only proves they compile and link)
- [ ] TUI parity with the GUI for newer features: emoji reactions,
      received positions, aliases / favorites, forward, telemetry panel
- [ ] Inline map view for received positions (currently just an
      OpenStreetMap link via the 📍 pill)
- [ ] Surface more Meshtastic telemetry variants in the stats panel
      (environment, power, health) — only DeviceMetrics is decoded today
- [ ] Investigate Meshcore v2 once it adds sender attribution on
      channel messages (currently rendered as synthetic `chan{N}`)

## License

GPL-3.0-or-later. See [LICENSE](LICENSE) for the full text. Imposed by
transitivity through the upstream `meshtastic` crate (GPL-3.0).
