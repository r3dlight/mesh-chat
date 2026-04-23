<script setup>
import { ref, computed, onMounted, onBeforeUnmount, nextTick } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
// Logo is in public/ so it's served as a plain static asset at its
// native MIME type. Going through a JS import (even with `?url`) breaks
// on the WebKit webview Tauri uses on Linux ("image/svg+xml is not a
// valid JavaScript MIME type").
const logoUrl = "/logo.svg";

const ports = ref([]);
const selectedPort = ref("");
const input = ref("");
const status = ref("disconnected");
const myId = ref(null);
const connected = ref(false);
const messages = ref([]);
const channels = ref({}); // index -> ChannelInfo
const nodes = ref({}); // node_id -> NodeInfo (for long-name lookup)
const dmThreads = ref({}); // peer_id -> ChatMessage[]
const dmUnread = ref({}); // peer_id -> unread count
const currentSpace = ref({ kind: "channel", idx: 0 }); // { kind: "channel", idx } | { kind: "dm", peer }
const messagesEl = ref(null);
const historyInfo = ref({
  encrypted: false,
  restored: 0,
  errors: 0,
});

// Which panel modal is open: "identity" | "channels" | "nodes" | "radio" | null.
const openPanel = ref(null);

// Last LoRa + Device config reported by the radio.
const loraInfo = ref(null);
const deviceRole = ref(null);

// Radio config editor form state.
const radioForm = ref({
  region: "",
  modem_preset: "",
  use_preset: true,
  hop_limit: 3,
  tx_enabled: true,
  tx_power: 22,
  role: "",
});
const radioBusy = ref(false);
const radioError = ref(null);
const radioConfirm = ref(false);

// Firmware enums the UI lets the user pick from. Keep in sync with
// `protobufs::config::lo_ra_config::{RegionCode, ModemPreset}` and
// `protobufs::config::device_config::Role` enum names.
const REGION_OPTIONS = [
  "UNSET", "US", "EU_433", "EU_868", "CN", "JP", "ANZ", "KR", "TW", "RU",
  "IN", "NZ_865", "TH", "LORA_24", "UA_433", "UA_868", "MY_433", "MY_919",
  "SG_923", "PH_433", "PH_868", "PH_915", "ANZ_433", "KZ_433", "KZ_863",
  "NP_865", "BR_902",
];
const PRESET_OPTIONS = [
  "LONG_FAST", "LONG_SLOW", "VERY_LONG_SLOW", "MEDIUM_SLOW", "MEDIUM_FAST",
  "SHORT_SLOW", "SHORT_FAST", "LONG_MODERATE", "SHORT_TURBO",
];
const ROLE_OPTIONS = [
  "CLIENT", "CLIENT_MUTE", "ROUTER", "ROUTER_CLIENT", "REPEATER",
  "TRACKER", "SENSOR", "TAK", "CLIENT_HIDDEN", "LOST_AND_FOUND",
  "TAK_TRACKER",
];

// Identity editor form state.
const identityLong = ref("");
const identityShort = ref("");
const identityBusy = ref(false);
const identityError = ref(null);

// Channel editor form state.
const editingChannel = ref(null); // null or an index (1-7)
const editName = ref("");
const editPsk = ref("random16");
const channelBusy = ref(false);
const channelError = ref(null);

const PSK_PRESETS = [
  { value: "random16", label: "random16 (AES-128 custom)" },
  { value: "random32", label: "random32 (AES-256 custom)" },
  { value: "custom", label: "custom — paste hex or base64 below" },
  { value: "default", label: "default (LongFast — public)" },
  { value: "default2", label: "default2 — public" },
  { value: "default3", label: "default3 — public" },
  { value: "default4", label: "default4 — public" },
  { value: "default5", label: "default5 — public" },
  { value: "default6", label: "default6 — public" },
  { value: "default7", label: "default7 — public" },
  { value: "default8", label: "default8 — public" },
  { value: "default9", label: "default9 — public" },
  { value: "default10", label: "default10 — public" },
  { value: "none", label: "none (no encryption)" },
];

// Extra state for the custom PSK double-entry flow.
const customPsk1 = ref("");
const customPsk2 = ref("");

// Channel share modal state.
const shareOpen = ref(false);
const shareData = ref({ url: "", qr_svg: "" });
const shareError = ref(null);
const shareBusy = ref(false);

// Per-user overrides: alias map + favorites set. Loaded once from Tauri
// on mount; every mutation goes back through set_alias / set_favorite so
// the backend persists it to aliases.json.
const aliases = ref({}); // { node_id: "Custom name" }
const favorites = ref({}); // { node_id: true } (object-as-set for reactivity)
const aliasEdit = ref({}); // per-row draft: { node_id: "current input" }

// Forward modal state.
const forwardOpen = ref(false);
const forwardText = ref("");
const forwardError = ref(null);
const forwardBusy = ref(false);

// Reply-compose state. When non-null, the composer shows a "replying to"
// bar with the quoted preview and the next send will carry reply_to_text.
// Shape: { author: string, text: string }.
const replyingTo = ref(null);

// Backend we're currently connected to — "meshtastic", "meshcore", or
// "none". Drives protocol-specific UI gating (e.g. emoji reactions are
// disabled on Meshcore because its companion protocol has no native
// reaction primitive).
const currentNetwork = ref("none");

// Emojis offered inline under each received-and-reactable bubble. The
// picker is not a popover — it's always visible on eligible messages so
// reactions are one click away (matches the user's preferred discovery
// pattern).
const REACTION_CHOICES = ["👍", "❤", "😂", "😮", "😢", "🎉", "👀", "🚀"];

// Latest telemetry snapshot per node-id. Meshtastic-only for now
// (Meshcore doesn't broadcast Telemetry packets in the companion proto).
// Shape per entry:
//   { battery, voltage, channelUtilization, airUtilTx, uptime, timestamp }
const telemetry = ref({});

// Last known position per node-id. Populated from Position events,
// rendered next to the node in the nodes modal and as an inline pill
// on the node's most recent bubble.
const positions = ref({});

// Position-share modal state.
const positionModalOpen = ref(false);
const positionForm = ref({ latitude: null, longitude: null });
const positionError = ref(null);
const positionBusy = ref(false);

// In-space search. Ctrl+F toggles the bar; Esc clears and closes.
// Filter runs inside `filteredMessages` so the match count is just the
// length of the result array.
const searchVisible = ref(false);
const searchQuery = ref("");
const searchInputEl = ref(null);

function openPositionModal() {
  positionError.value = null;
  positionBusy.value = false;
  // Try the browser's geolocation first; fall back to manual entry if
  // the user denies, the API is unavailable, or we time out.
  positionForm.value = { latitude: null, longitude: null };
  positionModalOpen.value = true;
  if (typeof navigator !== "undefined" && navigator.geolocation) {
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        positionForm.value = {
          latitude: Number(pos.coords.latitude.toFixed(6)),
          longitude: Number(pos.coords.longitude.toFixed(6)),
        };
      },
      (err) => {
        positionError.value = `geolocation unavailable: ${err.message || err.code}. Enter coordinates manually.`;
      },
      { enableHighAccuracy: false, timeout: 8000, maximumAge: 60_000 },
    );
  } else {
    positionError.value =
      "no geolocation API in this webview — enter coordinates manually.";
  }
}

async function submitPosition() {
  positionError.value = null;
  const { latitude, longitude } = positionForm.value;
  if (
    typeof latitude !== "number" ||
    typeof longitude !== "number" ||
    Number.isNaN(latitude) ||
    Number.isNaN(longitude)
  ) {
    positionError.value = "latitude and longitude must be numbers";
    return;
  }
  if (latitude < -90 || latitude > 90 || longitude < -180 || longitude > 180) {
    positionError.value = "out of WGS84 range";
    return;
  }
  positionBusy.value = true;
  try {
    await invoke("send_position", { latitude, longitude });
    positionModalOpen.value = false;
  } catch (e) {
    positionError.value = e?.message || String(e);
  } finally {
    positionBusy.value = false;
  }
}

function openSearch() {
  searchVisible.value = true;
  nextTick(() => {
    searchInputEl.value?.focus();
  });
}

function closeSearch() {
  searchVisible.value = false;
  searchQuery.value = "";
}

const MAX_REPLY_QUOTE = 140;
function truncateQuote(text) {
  if (!text) return "";
  const collapsed = text.replace(/\s+/g, " ").trim();
  if (collapsed.length <= MAX_REPLY_QUOTE) return collapsed;
  return collapsed.slice(0, MAX_REPLY_QUOTE - 1) + "…";
}

// Unlock modal state.
const historyState = ref(null); // populated by history_state()
const historyStateError = ref(null); // surfaced when the call fails or times out
const unlockPass = ref("");
const unlockPass2 = ref("");
const unlockError = ref(null);
const unlockBusy = ref(false);

const needsUnlock = computed(
  () => historyState.value && !historyState.value.unlocked,
);
const needsSetup = computed(() => historyState.value?.needs_setup);

let unlistenMesh = null;

// ─── Derived state ───────────────────────────────────────────────────────

const isChannelSpace = computed(() => currentSpace.value.kind === "channel");
const isDmSpace = computed(() => currentSpace.value.kind === "dm");

const filteredMessages = computed(() => {
  const base = isChannelSpace.value
    ? messages.value.filter(
        (m) => m.channel === currentSpace.value.idx && !isDirectMessage(m),
      )
    : dmThreads.value[currentSpace.value.peer] || [];
  if (!searchVisible.value || !searchQuery.value.trim()) return base;
  // Case-insensitive substring match, searching body text + reply quote
  // so threaded replies match on their parent too.
  const needle = searchQuery.value.trim().toLowerCase();
  return base.filter((m) => {
    const hay = `${m.text} ${m.replyToText || ""}`.toLowerCase();
    return hay.includes(needle);
  });
});

const currentChannelInfo = computed(() =>
  isChannelSpace.value ? channels.value[currentSpace.value.idx] : null,
);

const currentLabel = computed(() => {
  if (isChannelSpace.value) {
    return channelName(currentChannelInfo.value, currentSpace.value.idx);
  }
  return displayName(currentSpace.value.peer);
});

const isPrivateChannel = computed(() => {
  // DMs are end-to-end encrypted via firmware PKC — always private in the UI.
  if (isDmSpace.value) return true;
  const c = currentChannelInfo.value;
  if (!c) return false;
  return c.psk?.length === 16 || c.psk?.length === 32;
});

// Ordered list of spaces for the sidebar: channels (non-disabled) then DM
// threads (most recent first).
const allSpaces = computed(() => {
  const out = [];
  for (let i = 0; i < 8; i++) {
    const c = channels.value[i];
    if (c && c.role === "Disabled") continue;
    out.push({ kind: "channel", idx: i, info: c || null });
  }
  // Fallback when we haven't received any channel yet.
  if (out.length === 0) {
    out.push({ kind: "channel", idx: 0, info: null });
    out.push({ kind: "channel", idx: 1, info: null });
  }

  // Favorites float to the top, then recency-sorted non-favorites.
  const dmOrder = Object.entries(dmThreads.value)
    .map(([peer, msgs]) => ({
      peer,
      lastTs: msgs.length ? msgs[msgs.length - 1].timestamp : 0,
      fav: isFavorite(peer),
    }))
    .sort((a, b) => {
      if (a.fav !== b.fav) return a.fav ? -1 : 1;
      return b.lastTs - a.lastTs;
    });
  for (const { peer } of dmOrder) {
    out.push({ kind: "dm", peer });
  }
  return out;
});

function spaceKey(s) {
  return s.kind === "channel" ? `c:${s.idx}` : `d:${s.peer}`;
}

function isSameSpace(a, b) {
  if (a.kind !== b.kind) return false;
  return a.kind === "channel" ? a.idx === b.idx : a.peer === b.peer;
}

function switchSpace(space) {
  currentSpace.value = { ...space };
  if (space.kind === "dm") {
    delete dmUnread.value[space.peer];
    dmUnread.value = { ...dmUnread.value };
  }
  scrollToBottom();
}

function channelName(info, index) {
  if (info?.name) return info.name;
  if (info?.role === "Primary") return "default";
  return `ch${index}`;
}

function channelPrivate(info) {
  return info?.psk?.length === 16 || info?.psk?.length === 32;
}

function displayName(id) {
  // Lookup precedence: user alias → advertised long_name → raw id.
  if (!id) return "?";
  const custom = aliases.value[id];
  if (custom) return custom;
  const n = nodes.value[id];
  if (n && n.long_name) return n.long_name;
  return id;
}

function isFavorite(id) {
  return !!favorites.value[id];
}

function isDirectMessage(m) {
  // A received DM has `to` matching our own id; an outgoing echo has `to`
  // equal to the peer's node id (not "^all" and not broadcast).
  if (!myId.value) return false;
  if (m.to === "^all") return false;
  return m.to === myId.value || (m.from === myId.value && m.to.startsWith("!"));
}

function dmPeerOf(m) {
  // Which peer the DM belongs to, from our perspective.
  if (m.to === myId.value) return m.from;
  return m.to;
}

function meLabel() {
  const id = myId.value;
  if (!id) return "me";
  const n = nodes.value[id];
  if (n && n.long_name) return `me (${n.long_name})`;
  return "me";
}

// ─── Panel actions ──────────────────────────────────────────────────────

function openIdentityPanel() {
  identityError.value = null;
  const me = myId.value ? nodes.value[myId.value] : null;
  identityLong.value = me?.long_name ?? "";
  identityShort.value = me?.short_name ?? "";
  openPanel.value = "identity";
}

async function submitIdentity() {
  identityError.value = null;
  if (!identityLong.value.trim() || !identityShort.value.trim()) {
    identityError.value = "both names are required";
    return;
  }
  identityBusy.value = true;
  try {
    await invoke("update_user", {
      longName: identityLong.value,
      shortName: identityShort.value,
    });
    // Optimistic local update so the "me (longname)" label refreshes
    // immediately instead of waiting for the next NodeInfo broadcast.
    if (myId.value) {
      nodes.value = {
        ...nodes.value,
        [myId.value]: {
          ...(nodes.value[myId.value] || { id: myId.value }),
          long_name: identityLong.value,
          short_name: identityShort.value,
        },
      };
    }
    openPanel.value = null;
  } catch (e) {
    identityError.value = String(e);
  } finally {
    identityBusy.value = false;
  }
}

function startChannelEdit(index) {
  if (index === 0) return; // Primary is read-only
  channelError.value = null;
  const existing = channels.value[index];
  editName.value = existing?.name ?? "";
  editPsk.value = "random16";
  customPsk1.value = "";
  customPsk2.value = "";
  editingChannel.value = index;
}

async function submitChannelEdit() {
  channelError.value = null;
  if (editingChannel.value == null) return;
  if (!editName.value.trim()) {
    channelError.value = "name cannot be empty";
    return;
  }
  if (editName.value.length > 11) {
    channelError.value = "name too long (max 11 chars)";
    return;
  }
  channelBusy.value = true;
  try {
    if (editPsk.value === "custom") {
      if (!customPsk1.value) {
        throw new Error("paste your PSK (hex or base64)");
      }
      if (customPsk1.value !== customPsk2.value) {
        throw new Error("PSK confirmation does not match");
      }
      await invoke("upsert_channel_custom", {
        index: editingChannel.value,
        name: editName.value,
        psk: customPsk1.value,
        pskConfirm: customPsk2.value,
      });
    } else {
      await invoke("upsert_channel", {
        index: editingChannel.value,
        name: editName.value,
        pskPreset: editPsk.value,
      });
    }
    customPsk1.value = "";
    customPsk2.value = "";
    editingChannel.value = null;
  } catch (e) {
    channelError.value = e?.message || String(e);
  } finally {
    channelBusy.value = false;
  }
}

async function shareChannel(index) {
  const c = channels.value[index];
  if (!c) return;
  shareError.value = null;
  shareBusy.value = true;
  try {
    const res = await invoke("channel_share_fields", {
      name: c.name || (c.role === "Primary" ? "default" : `ch${index}`),
      psk: Array.from(c.psk || []),
      uplinkEnabled: c.uplink_enabled ?? true,
      downlinkEnabled: c.downlink_enabled ?? true,
    });
    shareData.value = res;
    shareOpen.value = true;
  } catch (e) {
    shareError.value = e?.message || String(e);
  } finally {
    shareBusy.value = false;
  }
}

async function copyShareUrl() {
  try {
    await navigator.clipboard.writeText(shareData.value.url);
    shareError.value = "copied ✓";
    setTimeout(() => (shareError.value = null), 1500);
  } catch (e) {
    shareError.value = `copy failed: ${e}`;
  }
}

async function deleteChannel(index) {
  if (index === 0) return;
  if (!confirm(`Delete channel #${index}?`)) return;
  try {
    await invoke("delete_channel", { index });
  } catch (e) {
    channelError.value = String(e);
  }
}

function pskPreview(psk) {
  if (!psk || psk.length === 0) return "none";
  if (psk.length === 1) return `default${psk[0]}`;
  if (psk.length === 16) return "AES-128 (custom)";
  if (psk.length === 32) return "AES-256 (custom)";
  return `${psk.length} bytes`;
}

function channelPrivacyTag(info) {
  return channelPrivate(info) ? "PRIVATE" : "PUBLIC";
}

// Nodes modal: sorted list with "Start DM" action.
const sortedNodes = computed(() => {
  return Object.values(nodes.value).sort((a, b) => {
    const at = a.last_heard || 0;
    const bt = b.last_heard || 0;
    return bt - at;
  });
});

function openRadioPanel() {
  radioError.value = null;
  radioConfirm.value = false;
  // Pre-fill from the last snapshot the radio sent us. Refuse to open the
  // editor if we haven't received it yet — writing blind is risky.
  if (!loraInfo.value) {
    radioError.value =
      "radio has not yet reported its LoRa config — wait a moment after connect";
    openPanel.value = "radio";
    return;
  }
  radioForm.value = {
    region: loraInfo.value.region,
    modem_preset: loraInfo.value.modem_preset,
    use_preset: loraInfo.value.use_preset,
    hop_limit: loraInfo.value.hop_limit || 3,
    tx_enabled: loraInfo.value.tx_enabled,
    tx_power: loraInfo.value.tx_power || 22,
    role: deviceRole.value || "CLIENT",
  };
  openPanel.value = "radio";
}

// Current snapshot vs form: computed so we can highlight what will change.
const radioDiff = computed(() => {
  if (!loraInfo.value) return [];
  const out = [];
  const f = radioForm.value;
  const l = loraInfo.value;
  if (f.region !== l.region) out.push(`region: ${l.region} → ${f.region}`);
  if (f.modem_preset !== l.modem_preset)
    out.push(`preset: ${l.modem_preset} → ${f.modem_preset}`);
  if (f.use_preset !== l.use_preset)
    out.push(`use_preset: ${l.use_preset} → ${f.use_preset}`);
  if (f.hop_limit !== l.hop_limit)
    out.push(`hop_limit: ${l.hop_limit} → ${f.hop_limit}`);
  if (f.tx_enabled !== l.tx_enabled)
    out.push(`tx_enabled: ${l.tx_enabled} → ${f.tx_enabled}`);
  if (f.tx_power !== l.tx_power)
    out.push(`tx_power: ${l.tx_power} → ${f.tx_power}dBm`);
  if (f.role !== (deviceRole.value || ""))
    out.push(`role: ${deviceRole.value || "?"} → ${f.role}`);
  return out;
});

async function submitRadioConfig() {
  radioError.value = null;
  const f = radioForm.value;
  // Client-side guardrails — backend re-validates, but surface errors
  // immediately in the UI.
  if (!REGION_OPTIONS.includes(f.region)) {
    radioError.value = `unknown region: ${f.region}`;
    return;
  }
  if (!PRESET_OPTIONS.includes(f.modem_preset)) {
    radioError.value = `unknown modem preset: ${f.modem_preset}`;
    return;
  }
  if (!ROLE_OPTIONS.includes(f.role)) {
    radioError.value = `unknown device role: ${f.role}`;
    return;
  }
  if (f.hop_limit < 0 || f.hop_limit > 7) {
    radioError.value = "hop_limit must be in 0..=7";
    return;
  }
  if (f.tx_power < 0 || f.tx_power > 30) {
    radioError.value = "tx_power must be in 0..=30 dBm";
    return;
  }
  if (radioDiff.value.length === 0) {
    radioError.value = "no changes";
    return;
  }
  // Step 1 → 2: require explicit confirm click before sending.
  if (!radioConfirm.value) {
    radioConfirm.value = true;
    return;
  }
  radioBusy.value = true;
  try {
    // LoRa first so the role change doesn't bounce the radio before we
    // land the new region / preset. If any command errors, stop here.
    await invoke("set_lora_config", {
      region: f.region,
      modemPreset: f.modem_preset,
      usePreset: f.use_preset,
      hopLimit: f.hop_limit,
      txEnabled: f.tx_enabled,
      txPower: f.tx_power,
    });
    if (f.role !== (deviceRole.value || "")) {
      await invoke("set_device_role", { role: f.role });
    }
    openPanel.value = null;
    radioConfirm.value = false;
  } catch (e) {
    radioError.value = e?.message || String(e);
  } finally {
    radioBusy.value = false;
  }
}

function positionOf(nodeId) {
  return positions.value[nodeId] || null;
}

function telemetryOf(nodeId) {
  return telemetry.value[nodeId] || null;
}

function fmtUptime(seconds) {
  if (seconds == null) return "—";
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

function fmtPercent(value) {
  return value == null ? "—" : `${value.toFixed(1)}%`;
}

const sortedTelemetryNodes = computed(() =>
  Object.entries(telemetry.value)
    .map(([id, t]) => ({ id, ...t }))
    .sort((a, b) => (b.timestamp || 0) - (a.timestamp || 0)),
);

function osmLink(lat, lon) {
  return `https://openstreetmap.org/?mlat=${lat}&mlon=${lon}&zoom=14`;
}

function applyReaction(r) {
  // Find the first message whose packetId matches the reaction's
  // `reply_to_packet_id`, then append the sender to that emoji's bucket.
  // We deduplicate by `from` so the same node reacting twice doesn't
  // inflate the count.
  const target = r.reply_to_packet_id;
  if (!target) return;
  const mutate = (arr) => {
    const m = arr.find((x) => x.packetId === target);
    if (!m) return false;
    const bucket = { ...(m.reactions || {}) };
    const list = Array.isArray(bucket[r.emoji]) ? [...bucket[r.emoji]] : [];
    if (!list.includes(r.from)) list.push(r.from);
    bucket[r.emoji] = list;
    m.reactions = bucket;
    return true;
  };
  if (mutate(messages.value)) return;
  for (const peer of Object.keys(dmThreads.value)) {
    if (mutate(dmThreads.value[peer])) return;
  }
}

function canReactTo(m) {
  return (
    currentNetwork.value === "meshtastic" &&
    !!m.packetId &&
    connected.value &&
    !m.isMe
  );
}

function reactionTooltip(m) {
  if (m.isMe) return "Can't react to your own messages";
  if (currentNetwork.value !== "meshtastic") {
    return "Reactions require Meshtastic (not supported on Meshcore)";
  }
  if (!m.packetId) return "No packet id — can't react to this message";
  return "React with an emoji";
}

async function pickReaction(m, emoji) {
  if (!canReactTo(m)) return;
  try {
    // DM reactions go to the other party; channel reactions are broadcast.
    const isDm = isDirectMessage(m);
    const to = isDm ? m.from : null;
    const channelIdx = isDm ? 0 : m.channel;
    await invoke("send_reaction", {
      channel: channelIdx,
      to,
      replyToPacketId: m.packetId,
      emoji,
    });
  } catch (e) {
    status.value = `reaction error: ${e}`;
  }
}

function startReply(m) {
  const author = m.isMe ? meLabel() : displayName(m.from);
  replyingTo.value = {
    author,
    text: truncateQuote(m.text),
  };
}

function cancelReply() {
  replyingTo.value = null;
}

function openForward(text) {
  if (!text) return;
  forwardText.value = text;
  forwardError.value = null;
  forwardOpen.value = true;
}

async function forwardTo(space) {
  if (!connected.value || forwardBusy.value) return;
  forwardBusy.value = true;
  forwardError.value = null;
  try {
    const isDm = space.kind === "dm";
    const channelIdx = isDm ? 0 : space.idx;
    const to = isDm ? space.peer : null;
    await invoke("send_text", {
      channel: channelIdx,
      text: forwardText.value,
      to,
    });
    // Jump to the destination so the user sees the forwarded bubble.
    switchSpace(space);
    forwardOpen.value = false;
  } catch (e) {
    forwardError.value = e?.message || String(e);
  } finally {
    forwardBusy.value = false;
  }
}

async function refreshAliases() {
  try {
    const snap = await invoke("get_aliases");
    aliases.value = snap.aliases || {};
    const favMap = {};
    for (const id of snap.favorites || []) favMap[id] = true;
    favorites.value = favMap;
  } catch (e) {
    console.warn("get_aliases failed:", e);
  }
}

async function commitAlias(nodeId) {
  const raw = aliasEdit.value[nodeId];
  const alias = raw && raw.trim() ? raw.trim() : null;
  try {
    await invoke("set_alias", { nodeId, alias });
    const copy = { ...aliases.value };
    if (alias) copy[nodeId] = alias;
    else delete copy[nodeId];
    aliases.value = copy;
    // Clear the draft so the input shows the committed value via the
    // displayName() fallback.
    delete aliasEdit.value[nodeId];
    aliasEdit.value = { ...aliasEdit.value };
  } catch (e) {
    console.warn("set_alias failed:", e);
  }
}

async function toggleFavorite(nodeId) {
  const next = !isFavorite(nodeId);
  try {
    await invoke("set_favorite", { nodeId, favorite: next });
    const copy = { ...favorites.value };
    if (next) copy[nodeId] = true;
    else delete copy[nodeId];
    favorites.value = copy;
  } catch (e) {
    console.warn("set_favorite failed:", e);
  }
}

function startDmWithNode(nodeId) {
  if (!nodeId || nodeId === myId.value) return;
  // Make sure the thread exists so the sidebar shows it.
  if (!dmThreads.value[nodeId]) {
    dmThreads.value = { ...dmThreads.value, [nodeId]: [] };
  }
  switchSpace({ kind: "dm", peer: nodeId });
  openPanel.value = null;
}

// ─── Desktop notifications ──────────────────────────────────────────────

let notifPermissionPromise = null;
async function ensureNotifPermission() {
  if (!notifPermissionPromise) {
    notifPermissionPromise = (async () => {
      try {
        let granted = await isPermissionGranted();
        if (!granted) {
          const res = await requestPermission();
          granted = res === "granted";
        }
        return granted;
      } catch (e) {
        console.warn("notification permission:", e);
        return false;
      }
    })();
  }
  return notifPermissionPromise;
}

function myLongName() {
  if (!myId.value) return null;
  return nodes.value[myId.value]?.long_name || null;
}

function looksMentioned(text) {
  const ln = myLongName();
  if (!ln) return false;
  // Case-insensitive substring — the common "hey r3dlight1 :)" style.
  return text.toLowerCase().includes(ln.toLowerCase());
}

async function maybeNotify(msg) {
  // Never notify for our own messages or when the window is focused.
  if (msg.isMe) return;
  if (typeof document !== "undefined" && document.hasFocus()) return;

  const isDm = isDirectMessage(msg);
  const mentioned = !isDm && looksMentioned(msg.text);
  if (!isDm && !mentioned) return;

  const granted = await ensureNotifPermission();
  if (!granted) return;

  const who = displayName(msg.from);
  const title = isDm ? `✉ DM from ${who}` : `@ mentioned by ${who}`;
  const body = msg.text.length > 140 ? msg.text.slice(0, 139) + "…" : msg.text;
  try {
    sendNotification({ title, body });
  } catch (e) {
    console.warn("sendNotification:", e);
  }
}

function relativeSeen(ts) {
  if (!ts) return "—";
  const delta = Math.floor(Date.now() / 1000) - ts;
  if (delta < 60) return `${delta}s`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h`;
  return `${Math.floor(delta / 86400)}d`;
}

// ─── Actions ─────────────────────────────────────────────────────────────

async function refreshPorts() {
  try {
    ports.value = await invoke("list_ports");
    if (ports.value.length > 0 && !selectedPort.value) {
      selectedPort.value = ports.value[0];
    }
  } catch (e) {
    status.value = `scan error: ${e}`;
  }
}

async function connect() {
  if (!selectedPort.value) return;
  status.value = `connecting to ${selectedPort.value}…`;
  try {
    await invoke("connect_device", { port: selectedPort.value });
    connected.value = true;
    status.value = "connected";
  } catch (e) {
    status.value = `error: ${e}`;
  }
}

async function refreshHistoryState() {
  historyStateError.value = null;
  try {
    // Time-box the call so the UI never sits on ⏳ loading forever if the
    // backend is misbehaving (wrong build, missing command registration,
    // bridge not ready, ...). 5s is generous for a no-op read.
    const timeout = new Promise((_, reject) =>
      setTimeout(
        () => reject(new Error("history_state timed out (5s)")),
        5000,
      ),
    );
    const s = await Promise.race([invoke("history_state"), timeout]);
    historyState.value = s;
    historyInfo.value.encrypted = s.encrypt_requested;
    console.log("history_state:", s);
  } catch (e) {
    const msg = e?.message || String(e);
    console.error("history_state failed:", e);
    historyState.value = null;
    historyStateError.value = msg;
    status.value = `history state error: ${msg}`;
  }
}

async function submitUnlock() {
  unlockError.value = null;
  if (!unlockPass.value) {
    unlockError.value = "passphrase cannot be empty";
    return;
  }
  if (needsSetup.value && unlockPass.value !== unlockPass2.value) {
    unlockError.value = "passphrases do not match";
    return;
  }
  unlockBusy.value = true;
  try {
    const res = await invoke("unlock_history", {
      passphrase: unlockPass.value,
    });
    historyInfo.value.restored = res.report.restored;
    historyInfo.value.errors = res.report.errors;
    unlockPass.value = "";
    unlockPass2.value = "";
    await refreshHistoryState();
  } catch (e) {
    unlockError.value = String(e);
  } finally {
    unlockBusy.value = false;
  }
}

async function send() {
  const text = input.value.trim();
  if (!text || !connected.value) return;
  try {
    const isDm = isDmSpace.value;
    const channelIdx = isDm ? 0 : currentSpace.value.idx;
    const to = isDm ? currentSpace.value.peer : null;
    const replyToText = replyingTo.value
      ? `@${replyingTo.value.author}: ${replyingTo.value.text}`
      : null;
    await invoke("send_text", {
      channel: channelIdx,
      text,
      to,
      replyToText,
    });
    input.value = "";
    replyingTo.value = null;
  } catch (e) {
    status.value = `send error: ${e}`;
  }
}

function scrollToBottom() {
  nextTick(() => {
    const el = messagesEl.value;
    if (el) el.scrollTop = el.scrollHeight;
  });
}

function fmtTime(ts) {
  return new Date(ts * 1000).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

// ─── Event handling ──────────────────────────────────────────────────────

function handleMeshEvent(evt) {
  if (evt.Connected) {
    myId.value = evt.Connected.my_id;
    // Backend type comes through here — lets the UI gate protocol-specific
    // features immediately without a separate round-trip.
    if (evt.Connected.network) {
      currentNetwork.value = String(evt.Connected.network).toLowerCase();
    }
    status.value = `connected · ${evt.Connected.my_id}`;
  } else if (evt.Disconnected) {
    connected.value = false;
    status.value = "disconnected";
    myId.value = null;
  } else if (evt.TextMessage) {
    const m = evt.TextMessage;
    // "from" matches our id, OR "from == 'me'" if the backend echoed a
    // send before we had learned our node id, OR a localId is set (we
    // generated it on this client).
    const isMe =
      (myId.value && m.from === myId.value) ||
      m.from === "me" ||
      m.local_id != null;
    const msg = {
      channel: m.channel,
      from: m.from,
      to: m.to,
      text: m.text,
      timestamp: m.timestamp,
      isMe,
      localId: m.local_id,
      sendStatus: m.status,
      rxSnr: m.rx_snr,
      rxRssi: m.rx_rssi,
      replyToText: m.reply_to_text || null,
      packetId: m.packet_id ?? null,
      // { emoji: [from, ...] } — grows as Reaction events arrive.
      reactions: m.reactions || {},
    };
    if (isDirectMessage(msg)) {
      const peer = dmPeerOf(msg);
      const arr = dmThreads.value[peer] || [];
      dmThreads.value = { ...dmThreads.value, [peer]: [...arr, msg] };
      const viewing =
        isDmSpace.value && currentSpace.value.peer === peer;
      if (!viewing && !msg.isMe) {
        dmUnread.value = {
          ...dmUnread.value,
          [peer]: (dmUnread.value[peer] || 0) + 1,
        };
      }
    } else {
      messages.value.push(msg);
    }
    scrollToBottom();
    maybeNotify(msg);
  } else if (evt.NodeSeen) {
    const n = evt.NodeSeen;
    nodes.value = { ...nodes.value, [n.id]: n };
  } else if (evt.ChannelInfo) {
    const c = evt.ChannelInfo;
    channels.value = { ...channels.value, [c.index]: c };
  } else if (evt.SendResult) {
    const r = evt.SendResult;
    const update = (arr) => {
      const m = arr.find((x) => x.localId === r.local_id);
      if (m) m.sendStatus = r.ok ? "Sent" : { Failed: r.error };
    };
    update(messages.value);
    for (const peer of Object.keys(dmThreads.value)) {
      update(dmThreads.value[peer]);
    }
  } else if (evt.SendAck) {
    const r = evt.SendAck;
    const update = (arr) => {
      const m = arr.find((x) => x.localId === r.local_id);
      if (!m) return;
      // Never overwrite a terminal local Failed with a downstream Delivered.
      if (r.delivered && m.sendStatus?.Failed) return;
      m.sendStatus = r.delivered
        ? "Delivered"
        : { Failed: r.error || "routing failure" };
    };
    update(messages.value);
    for (const peer of Object.keys(dmThreads.value)) {
      update(dmThreads.value[peer]);
    }
  } else if (evt.LoraInfo) {
    loraInfo.value = evt.LoraInfo;
  } else if (evt.DeviceRoleInfo) {
    deviceRole.value = evt.DeviceRoleInfo.role;
  } else if (evt.Reaction) {
    applyReaction(evt.Reaction);
  } else if (evt.Position) {
    const p = evt.Position;
    positions.value = {
      ...positions.value,
      [p.from]: {
        latitude: p.latitude,
        longitude: p.longitude,
        timestamp: p.timestamp,
      },
    };
  } else if (evt.Telemetry) {
    const t = evt.Telemetry;
    telemetry.value = {
      ...telemetry.value,
      [t.from]: {
        battery: t.battery_level,
        voltage: t.voltage,
        channelUtilization: t.channel_utilization,
        airUtilTx: t.air_util_tx,
        uptime: t.uptime_seconds,
        timestamp: t.timestamp,
      },
    };
  } else if (evt.Error) {
    status.value = `error: ${evt.Error.message}`;
  } else if (evt.ConfigComplete) {
    if (status.value.startsWith("connected")) status.value = "ready";
  }
}

// Global keyboard handler mirrors the TUI shortcuts.
// Intentionally skipped when a modal is open or the user is typing in
// any input/textarea — typing a letter in the chat composer must never
// open a modal.
function handleGlobalKey(ev) {
  const tag = document.activeElement?.tagName;
  const inInput = tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
  // Needs-unlock and panel modals eat all shortcuts.
  if (needsUnlock.value || openPanel.value) return;

  // Ctrl+F (or Cmd+F on macOS) toggles the in-space search bar. Works
  // even when typing in the composer so users can jump into search
  // mid-sentence.
  if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "f") {
    ev.preventDefault();
    if (searchVisible.value) closeSearch();
    else openSearch();
    return;
  }

  // Esc: close search first, then any open panel (defensive).
  if (ev.key === "Escape") {
    if (searchVisible.value) {
      closeSearch();
      ev.preventDefault();
      return;
    }
    if (openPanel.value) {
      openPanel.value = null;
      ev.preventDefault();
    }
    return;
  }

  // Tab / Shift-Tab cycles spaces — but only when no input is focused.
  if (ev.key === "Tab" && !inInput && connected.value) {
    const spaces = allSpaces.value;
    if (!spaces.length) return;
    const pos = spaces.findIndex((s) => isSameSpace(currentSpace.value, s));
    const next = ev.shiftKey
      ? (pos - 1 + spaces.length) % spaces.length
      : (pos + 1) % spaces.length;
    switchSpace(spaces[next]);
    ev.preventDefault();
    return;
  }

  // Single-letter shortcuts: only when not typing in the composer.
  if (inInput && tag !== "BODY") return;

  if (!connected.value) return;
  switch (ev.key) {
    case "n":
      openPanel.value = "nodes";
      ev.preventDefault();
      break;
    case "c":
      openPanel.value = "channels";
      ev.preventDefault();
      break;
    case "i":
      // 'i' for identity (avoids clashing with "me" / "messages" initials).
      openIdentityPanel();
      ev.preventDefault();
      break;
    case "r":
      openRadioPanel();
      ev.preventDefault();
      break;
    case "d": {
      // Jump to most recent DM if any.
      const threads = Object.entries(dmThreads.value)
        .map(([peer, msgs]) => ({
          peer,
          lastTs: msgs.length ? msgs[msgs.length - 1].timestamp : 0,
        }))
        .sort((a, b) => b.lastTs - a.lastTs);
      if (threads.length > 0) {
        switchSpace({ kind: "dm", peer: threads[0].peer });
        ev.preventDefault();
      }
      break;
    }
    default:
      break;
  }
}

onMounted(async () => {
  // Each of these can fail independently (e.g. listen() throws if the
  // Tauri bridge isn't ready yet) — don't let one blow up the others.
  try {
    unlistenMesh = await listen("mesh-event", (e) => handleMeshEvent(e.payload));
  } catch (e) {
    console.error("listen mesh-event failed:", e);
  }
  await refreshHistoryState();
  try {
    await refreshPorts();
  } catch (e) {
    console.error("refreshPorts failed:", e);
  }
  await refreshAliases();
  try {
    currentNetwork.value = await invoke("get_network");
  } catch (e) {
    console.warn("get_network failed:", e);
  }
  window.addEventListener("keydown", handleGlobalKey);
});

onBeforeUnmount(() => {
  if (unlistenMesh) unlistenMesh();
  window.removeEventListener("keydown", handleGlobalKey);
});
</script>

<template>
  <div class="shell">
    <!-- Unlock / setup modal (blocks the rest of the UI until resolved) -->
    <div v-if="needsUnlock" class="unlock-overlay">
      <form class="unlock-card" @submit.prevent="submitUnlock">
        <div class="unlock-brand">
          <img :src="logoUrl" alt="mesh-chat" class="unlock-logo" />
          <div class="unlock-wordmark">mesh-chat</div>
        </div>
        <div class="unlock-title">
          {{ needsSetup ? "Set history passphrase" : "Unlock history" }}
        </div>
        <p class="unlock-hint">
          <template v-if="needsSetup">
            Choose a passphrase. It is never written to disk — you'll be
            asked for it every time you launch the app. Losing it means
            losing the history.
          </template>
          <template v-else-if="historyState?.has_legacy_v1">
            Existing history is in the legacy v1 format (key-file). No
            automatic migration — move the file aside to start fresh.
          </template>
          <template v-else-if="historyState?.has_legacy_plaintext">
            A plaintext history file exists but config requested encryption.
            Move it aside or disable encryption before continuing.
          </template>
          <template v-else>
            Enter the passphrase you set previously.
          </template>
        </p>
        <input
          type="password"
          v-model="unlockPass"
          :placeholder="needsSetup ? 'new passphrase' : 'passphrase'"
          autocomplete="off"
          autofocus
          :disabled="
            unlockBusy ||
            historyState?.has_legacy_v1 ||
            historyState?.has_legacy_plaintext
          "
        />
        <input
          v-if="needsSetup"
          type="password"
          v-model="unlockPass2"
          placeholder="confirm passphrase"
          autocomplete="off"
          :disabled="unlockBusy"
        />
        <div v-if="unlockError" class="unlock-error">⚠ {{ unlockError }}</div>
        <button
          class="btn-primary"
          type="submit"
          :disabled="
            unlockBusy ||
            historyState?.has_legacy_v1 ||
            historyState?.has_legacy_plaintext
          "
        >
          {{ unlockBusy ? "deriving key…" : needsSetup ? "Create" : "Unlock" }}
        </button>
      </form>
    </div>

    <!-- ─── Panel modals ────────────────────────────────────────────── -->

    <!-- Identity editor -->
    <div
      v-if="openPanel === 'identity'"
      class="panel-overlay"
      @click.self="openPanel = null"
    >
      <form class="panel-card" @submit.prevent="submitIdentity">
        <div class="panel-head">
          <h3>👤 Node identity</h3>
          <button type="button" class="panel-x" @click="openPanel = null">
            ✕
          </button>
        </div>
        <p class="panel-hint">
          Broadcast over the mesh via periodic NodeInfo packets. Long name
          up to 39 chars, short name up to 4.
        </p>
        <label class="field">
          <span>Long name</span>
          <input
            v-model="identityLong"
            maxlength="39"
            autofocus
            :disabled="identityBusy"
          />
          <span class="field-count">{{ identityLong.length }}/39</span>
        </label>
        <label class="field">
          <span>Short name</span>
          <input
            v-model="identityShort"
            maxlength="4"
            :disabled="identityBusy"
          />
          <span class="field-count">{{ identityShort.length }}/4</span>
        </label>
        <div v-if="identityError" class="unlock-error">⚠ {{ identityError }}</div>
        <div class="panel-actions">
          <button type="button" @click="openPanel = null">Cancel</button>
          <button type="submit" class="btn-primary" :disabled="identityBusy">
            {{ identityBusy ? "Saving…" : "Save" }}
          </button>
        </div>
      </form>
    </div>

    <!-- Channels manager -->
    <div
      v-if="openPanel === 'channels'"
      class="panel-overlay"
      @click.self="openPanel = null"
    >
      <div class="panel-card panel-card-lg">
        <div class="panel-head">
          <h3># Channels</h3>
          <button type="button" class="panel-x" @click="openPanel = null">
            ✕
          </button>
        </div>
        <p class="panel-hint">
          Primary (#0) is read-only. Secondary channels carry their own PSK
          (custom keys give real privacy; <code>default*</code> keys are
          public-known). Random presets are CSPRNG-generated in Rust.
        </p>
        <table class="chan-table">
          <thead>
            <tr>
              <th>#</th>
              <th>name</th>
              <th>role</th>
              <th>PSK</th>
              <th class="chan-actions-col"></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="i in 8" :key="i - 1" :class="{ disabled: !channels[i - 1] || channels[i - 1].role === 'Disabled' }">
              <td>{{ i - 1 }}</td>
              <td>
                {{ channels[i - 1]?.name || (channels[i - 1]?.role === "Primary" ? "default" : "—") }}
              </td>
              <td>
                <span
                  class="role-pill"
                  :class="{
                    primary: channels[i - 1]?.role === 'Primary',
                    secondary: channels[i - 1]?.role === 'Secondary',
                  }"
                >
                  {{ channels[i - 1]?.role || "—" }}
                </span>
              </td>
              <td>
                <span :class="channelPrivate(channels[i - 1]) ? 'tag-success-sm' : 'tag-danger-sm'">
                  {{ channelPrivacyTag(channels[i - 1]) }}
                </span>
                <span class="psk-preview">{{ pskPreview(channels[i - 1]?.psk) }}</span>
              </td>
              <td class="chan-actions">
                <button
                  v-if="i - 1 !== 0"
                  type="button"
                  @click="startChannelEdit(i - 1)"
                >
                  {{ channels[i - 1] && channels[i - 1].role !== "Disabled" ? "Edit" : "Create" }}
                </button>
                <button
                  v-if="channels[i - 1] && channels[i - 1].role !== 'Disabled'"
                  type="button"
                  @click="shareChannel(i - 1)"
                  title="Share as URL / QR"
                >
                  Share
                </button>
                <button
                  v-if="i - 1 !== 0 && channels[i - 1] && channels[i - 1].role === 'Secondary'"
                  type="button"
                  class="btn-danger"
                  @click="deleteChannel(i - 1)"
                >
                  Delete
                </button>
              </td>
            </tr>
          </tbody>
        </table>

        <!-- Inline edit form -->
        <form
          v-if="editingChannel != null"
          class="chan-edit"
          @submit.prevent="submitChannelEdit"
        >
          <h4>Edit channel #{{ editingChannel }}</h4>
          <label class="field">
            <span>Name</span>
            <input v-model="editName" maxlength="11" :disabled="channelBusy" />
            <span class="field-count">{{ editName.length }}/11</span>
          </label>
          <label class="field">
            <span>PSK</span>
            <select v-model="editPsk" :disabled="channelBusy">
              <option
                v-for="opt in PSK_PRESETS"
                :key="opt.value"
                :value="opt.value"
              >
                {{ opt.label }}
              </option>
            </select>
          </label>
          <template v-if="editPsk === 'custom'">
            <label class="field">
              <span>Paste PSK</span>
              <input
                type="password"
                v-model="customPsk1"
                autocomplete="off"
                spellcheck="false"
                placeholder="hex (32 or 64 chars) or base64 (16 or 32 bytes)"
                :disabled="channelBusy"
              />
            </label>
            <label class="field">
              <span>Confirm</span>
              <input
                type="password"
                v-model="customPsk2"
                autocomplete="off"
                spellcheck="false"
                placeholder="retype the exact same PSK"
                :disabled="channelBusy"
              />
            </label>
            <p class="panel-hint" style="margin-top: -0.2rem">
              Input is masked. Bytes are never logged, and the PSK is only
              sent to the radio — never echoed back to the UI.
            </p>
          </template>
          <div v-if="channelError" class="unlock-error">⚠ {{ channelError }}</div>
          <div class="panel-actions">
            <button type="button" @click="editingChannel = null">Cancel</button>
            <button type="submit" class="btn-primary" :disabled="channelBusy">
              {{ channelBusy ? "Writing…" : "Write to radio" }}
            </button>
          </div>
        </form>
      </div>
    </div>

    <!-- Channel share modal -->
    <div
      v-if="shareOpen"
      class="panel-overlay"
      @click.self="shareOpen = false"
    >
      <div class="panel-card">
        <div class="panel-head">
          <h3>✉ Share channel</h3>
          <button type="button" class="panel-x" @click="shareOpen = false">
            ✕
          </button>
        </div>
        <p class="panel-hint">
          Scan the QR code on another Meshtastic device, or share the
          link. Importing it adds this channel (same name + PSK) to the
          recipient's radio.
        </p>
        <div class="qr-wrap" v-html="shareData.qr_svg" />
        <label class="field">
          <span>URL</span>
          <input type="text" :value="shareData.url" readonly />
        </label>
        <div v-if="shareError" class="unlock-error">
          {{ shareError.includes("copied") ? "✓" : "⚠" }} {{ shareError }}
        </div>
        <div class="panel-actions">
          <button type="button" @click="shareOpen = false">Close</button>
          <button type="button" class="btn-primary" @click="copyShareUrl">
            Copy URL
          </button>
        </div>
      </div>
    </div>

    <!-- Radio config modal -->
    <div
      v-if="openPanel === 'radio'"
      class="panel-overlay"
      @click.self="openPanel = null"
    >
      <form class="panel-card panel-card-lg" @submit.prevent="submitRadioConfig">
        <div class="panel-head">
          <h3>⚙ Radio config</h3>
          <button type="button" class="panel-x" @click="openPanel = null">
            ✕
          </button>
        </div>
        <div class="radio-warn">
          ⚠ Writing wrong values can silence the radio or violate local
          regulations. The region must match the country you operate in.
          Changing region or preset reboots the device.
        </div>
        <template v-if="loraInfo">
          <div class="radio-grid">
            <label class="field">
              <span>Region</span>
              <select v-model="radioForm.region" :disabled="radioBusy || radioConfirm">
                <option v-for="r in REGION_OPTIONS" :key="r" :value="r">{{ r }}</option>
              </select>
            </label>
            <label class="field">
              <span>Modem preset</span>
              <select v-model="radioForm.modem_preset" :disabled="radioBusy || radioConfirm">
                <option v-for="p in PRESET_OPTIONS" :key="p" :value="p">{{ p }}</option>
              </select>
            </label>
            <label class="field">
              <span>Device role</span>
              <select v-model="radioForm.role" :disabled="radioBusy || radioConfirm">
                <option v-for="r in ROLE_OPTIONS" :key="r" :value="r">{{ r }}</option>
              </select>
            </label>
            <label class="field">
              <span>Use preset</span>
              <input type="checkbox" v-model="radioForm.use_preset" :disabled="radioBusy || radioConfirm" />
            </label>
            <label class="field">
              <span>Hop limit</span>
              <input
                type="number"
                min="0"
                max="7"
                v-model.number="radioForm.hop_limit"
                :disabled="radioBusy || radioConfirm"
              />
            </label>
            <label class="field">
              <span>TX enabled</span>
              <input type="checkbox" v-model="radioForm.tx_enabled" :disabled="radioBusy || radioConfirm" />
            </label>
            <label class="field">
              <span>TX power (dBm)</span>
              <input
                type="number"
                min="0"
                max="30"
                v-model.number="radioForm.tx_power"
                :disabled="radioBusy || radioConfirm"
              />
            </label>
          </div>
          <div v-if="radioDiff.length > 0" class="radio-diff">
            <div class="radio-diff-title">Pending changes</div>
            <ul>
              <li v-for="(d, i) in radioDiff" :key="i">{{ d }}</li>
            </ul>
          </div>
          <div v-else class="panel-hint">No changes.</div>
        </template>
        <template v-else>
          <p class="panel-hint">
            Waiting for the radio to report its current configuration…
          </p>
        </template>
        <div v-if="radioError" class="unlock-error">⚠ {{ radioError }}</div>
        <div class="panel-actions">
          <button type="button" @click="openPanel = null">Cancel</button>
          <button
            type="submit"
            class="btn-primary"
            :class="{ 'btn-danger': radioConfirm }"
            :disabled="radioBusy || !loraInfo || radioDiff.length === 0"
          >
            {{
              radioBusy
                ? "Writing…"
                : radioConfirm
                  ? "Yes, write to radio"
                  : "Review changes"
            }}
          </button>
        </div>
      </form>
    </div>

    <!-- Share-position modal -->
    <div
      v-if="positionModalOpen"
      class="panel-overlay"
      @click.self="positionModalOpen = false"
    >
      <form class="panel-card" @submit.prevent="submitPosition">
        <div class="panel-head">
          <h3>📍 Share position</h3>
          <button
            type="button"
            class="panel-x"
            @click="positionModalOpen = false"
          >
            ✕
          </button>
        </div>
        <p class="panel-hint">
          Broadcast your coordinates on channel 0. Decimal degrees,
          WGS84. Geolocation is fetched from the OS where available —
          override manually if the autofill is off.
        </p>
        <label class="field">
          <span>Latitude</span>
          <input
            type="number"
            step="0.000001"
            min="-90"
            max="90"
            v-model.number="positionForm.latitude"
            :disabled="positionBusy"
            placeholder="e.g. 48.858844"
          />
        </label>
        <label class="field">
          <span>Longitude</span>
          <input
            type="number"
            step="0.000001"
            min="-180"
            max="180"
            v-model.number="positionForm.longitude"
            :disabled="positionBusy"
            placeholder="e.g. 2.294351"
          />
        </label>
        <a
          v-if="
            typeof positionForm.latitude === 'number' &&
            typeof positionForm.longitude === 'number'
          "
          class="panel-hint"
          :href="`https://openstreetmap.org/?mlat=${positionForm.latitude}&mlon=${positionForm.longitude}&zoom=14`"
          target="_blank"
          rel="noopener"
        >
          Preview on OpenStreetMap ↗
        </a>
        <div v-if="positionError" class="unlock-error">⚠ {{ positionError }}</div>
        <div class="panel-actions">
          <button type="button" @click="positionModalOpen = false">Cancel</button>
          <button type="submit" class="btn-primary" :disabled="positionBusy">
            {{ positionBusy ? "Sending…" : "Broadcast" }}
          </button>
        </div>
      </form>
    </div>

    <!-- Forward message modal -->
    <div
      v-if="forwardOpen"
      class="panel-overlay"
      @click.self="forwardOpen = false"
    >
      <div class="panel-card">
        <div class="panel-head">
          <h3>↗ Forward message</h3>
          <button type="button" class="panel-x" @click="forwardOpen = false">
            ✕
          </button>
        </div>
        <p class="panel-hint">Pick a destination space. The message will be sent as-is, attributed to you.</p>
        <div class="forward-preview">{{ forwardText }}</div>
        <ul class="forward-list">
          <li
            v-for="s in allSpaces"
            :key="spaceKey(s)"
            class="forward-item"
            :class="{ disabled: forwardBusy || !connected }"
            @click="forwardTo(s)"
          >
            <template v-if="s.kind === 'channel'">
              <span class="priv-dot" :class="channelPrivate(s.info) ? 'dot-private' : 'dot-public'" />
              <span>{{ channelName(s.info, s.idx) }}</span>
              <span class="forward-meta">#{{ s.idx }}</span>
            </template>
            <template v-else>
              <span class="priv-dot dot-private" />
              <span class="dm-icon">✉</span>
              <span>{{ displayName(s.peer) }}</span>
              <span v-if="isFavorite(s.peer)" class="dm-fav">★</span>
              <span class="forward-meta">DM</span>
            </template>
          </li>
        </ul>
        <div v-if="forwardError" class="unlock-error">⚠ {{ forwardError }}</div>
        <div class="panel-actions">
          <button type="button" @click="forwardOpen = false">Cancel</button>
        </div>
      </div>
    </div>

    <!-- Stats / telemetry modal -->
    <div
      v-if="openPanel === 'stats'"
      class="panel-overlay"
      @click.self="openPanel = null"
    >
      <div class="panel-card panel-card-lg">
        <div class="panel-head">
          <h3>📊 Radio telemetry</h3>
          <button type="button" class="panel-x" @click="openPanel = null">
            ✕
          </button>
        </div>
        <p class="panel-hint">
          Latest device metrics broadcast by each node (Meshtastic
          `TelemetryApp` packets). Meshcore nodes do not expose this
          over the companion protocol, so they won't appear here.
        </p>
        <table class="nodes-table">
          <thead>
            <tr>
              <th>node</th>
              <th>battery</th>
              <th>voltage</th>
              <th>chan util</th>
              <th>TX util</th>
              <th>uptime</th>
              <th>seen</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="t in sortedTelemetryNodes" :key="t.id" :class="{ self: t.id === myId }">
              <td>
                <span v-if="t.id === myId" class="self-badge">●</span>
                {{ displayName(t.id) }}
              </td>
              <td>
                <span v-if="t.battery == null">—</span>
                <span v-else-if="t.battery > 100">⚡PWR</span>
                <span v-else>{{ t.battery }}%</span>
              </td>
              <td>{{ t.voltage == null ? "—" : `${t.voltage.toFixed(2)}V` }}</td>
              <td>{{ fmtPercent(t.channelUtilization) }}</td>
              <td>{{ fmtPercent(t.airUtilTx) }}</td>
              <td>{{ fmtUptime(t.uptime) }}</td>
              <td>{{ relativeSeen(t.timestamp) }}</td>
            </tr>
            <tr v-if="sortedTelemetryNodes.length === 0">
              <td colspan="7" class="empty-row">
                No telemetry packets received yet. Meshtastic nodes
                broadcast device metrics every ~30min by default.
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <!-- Nodes modal -->
    <div
      v-if="openPanel === 'nodes'"
      class="panel-overlay"
      @click.self="openPanel = null"
    >
      <div class="panel-card panel-card-lg">
        <div class="panel-head">
          <h3>⧉ Nodes ({{ sortedNodes.length }})</h3>
          <button type="button" class="panel-x" @click="openPanel = null">
            ✕
          </button>
        </div>
        <p class="panel-hint">
          All nodes heard on the mesh, sorted by last-heard.
          <strong>Click Start DM</strong> to open an end-to-end encrypted
          thread with a peer.
        </p>
        <table class="nodes-table">
          <thead>
            <tr>
              <th></th>
              <th>id</th>
              <th>name</th>
              <th>alias</th>
              <th>batt</th>
              <th>SNR</th>
              <th>hops</th>
              <th>seen</th>
              <th>pos</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="n in sortedNodes"
              :key="n.id"
              :class="{ self: n.id === myId }"
            >
              <td>
                <button
                  type="button"
                  class="fav-btn"
                  :class="{ active: isFavorite(n.id) }"
                  :title="isFavorite(n.id) ? 'Unfavorite' : 'Favorite — pin to top'"
                  @click="toggleFavorite(n.id)"
                >
                  {{ isFavorite(n.id) ? "★" : "☆" }}
                </button>
              </td>
              <td class="mono">{{ n.id }}</td>
              <td>
                <span v-if="n.id === myId" class="self-badge">●</span>
                {{ n.long_name || "—" }}
                <span class="short">{{ n.short_name ? `(${n.short_name})` : "" }}</span>
              </td>
              <td>
                <input
                  type="text"
                  class="alias-input"
                  :value="aliasEdit[n.id] ?? aliases[n.id] ?? ''"
                  :placeholder="aliases[n.id] ? '' : 'rename…'"
                  @input="aliasEdit[n.id] = $event.target.value"
                  @blur="commitAlias(n.id)"
                  @keydown.enter.prevent="commitAlias(n.id)"
                />
              </td>
              <td>
                <span v-if="n.battery_level == null">—</span>
                <span v-else-if="n.battery_level > 100">⚡PWR</span>
                <span v-else>{{ n.battery_level }}%</span>
              </td>
              <td>
                <span v-if="n.snr == null">—</span>
                <span v-else>{{ (n.snr >= 0 ? "+" : "") + n.snr.toFixed(1) }}dB</span>
              </td>
              <td>{{ n.hops_away == null ? "—" : n.hops_away }}</td>
              <td>{{ relativeSeen(n.last_heard) }}</td>
              <td>
                <a
                  v-if="positionOf(n.id)"
                  :href="osmLink(positionOf(n.id).latitude, positionOf(n.id).longitude)"
                  target="_blank"
                  rel="noopener"
                  class="position-link"
                  :title="`${positionOf(n.id).latitude.toFixed(5)}, ${positionOf(n.id).longitude.toFixed(5)}`"
                >
                  📍
                </a>
                <span v-else>—</span>
              </td>
              <td>
                <button
                  v-if="n.id !== myId"
                  type="button"
                  class="btn-primary sm"
                  @click="startDmWithNode(n.id)"
                >
                  ✉ Start DM
                </button>
              </td>
            </tr>
            <tr v-if="sortedNodes.length === 0">
              <td colspan="10" class="empty-row">
                No nodes heard yet — waiting on first NodeInfo packets.
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <!-- Top bar -->
    <header class="topbar">
      <div class="brand-block">
        <img :src="logoUrl" alt="" class="brand-logo" />
        <span class="brand-label">mesh-chat</span>
      </div>
      <div class="id-block">
        <span class="id-label">you</span>
        <span class="id-value">{{ myId ?? "—" }}</span>
      </div>

      <div class="spacer" />

      <div class="chips">
        <span v-if="!historyState" class="chip chip-muted" title="Waiting for history state…">
          ⏳ loading
        </span>
        <span
          v-if="historyState"
          class="chip"
          :class="
            historyState.encrypt_requested
              ? historyInfo.errors > 0
                ? 'chip-danger'
                : 'chip-success'
              : 'chip-muted'
          "
          :title="
            historyState.encrypt_requested
              ? historyInfo.errors > 0
                ? `history encrypted · ${historyInfo.errors} decrypt errors`
                : historyState.unlocked
                  ? 'history encrypted ✓ unlocked'
                  : 'history encrypted — locked'
              : 'history is plaintext (set [history] encrypt = true in config.toml)'
          "
        >
          {{
            historyState.encrypt_requested
              ? historyState.unlocked
                ? "🔒 history"
                : "🔒 locked"
              : "📄 plaintext"
          }}
        </span>
        <span
          v-if="connected"
          class="chip"
          :class="isPrivateChannel ? 'chip-success' : 'chip-danger'"
        >
          {{ isPrivateChannel ? "private" : "public" }}
        </span>
        <span class="chip chip-muted" :class="{ 'chip-live': connected }">
          <span class="pulse" />
          {{ status }}
        </span>
      </div>
    </header>

    <!-- Body -->
    <main class="body">
      <!-- Left rail -->
      <aside class="sidebar">
        <div
          v-if="!connected && historyState && historyState.unlocked"
          class="connect-card"
        >
          <h3>Connect</h3>
          <select v-model="selectedPort" class="full">
            <option v-for="p in ports" :key="p" :value="p">{{ p }}</option>
            <option v-if="ports.length === 0" disabled>(no ports detected)</option>
          </select>
          <div class="row">
            <button class="full" @click="refreshPorts">Rescan</button>
            <button
              class="full btn-primary"
              @click="connect"
              :disabled="!selectedPort"
            >
              Connect
            </button>
          </div>
        </div>

        <div v-else-if="!historyState" class="connect-card">
          <h3 v-if="!historyStateError">Loading</h3>
          <h3 v-else style="color: var(--danger)">History state error</h3>
          <p class="unlock-hint" v-if="!historyStateError">
            Reading history state…
          </p>
          <p class="unlock-hint" v-else>
            <strong style="color: var(--danger)">{{ historyStateError }}</strong
            ><br /><br />
            The Tauri command <code>history_state</code> didn't return. Open
            the webview devtools (Ctrl+Shift+I) and check the console for
            details, and the terminal running <code>cargo tauri dev</code>
            for Rust panics.
          </p>
          <button class="full" @click="refreshHistoryState">Retry</button>
        </div>

        <template v-else>
          <div class="panel-toolbar">
            <button
              class="tb-btn"
              @click="openIdentityPanel"
              title="Edit node name"
              :disabled="!connected"
            >
              👤 me
            </button>
            <button
              class="tb-btn"
              @click="openPanel = 'channels'"
              title="Manage channels"
              :disabled="!connected"
            >
              # chans
            </button>
            <button
              class="tb-btn"
              @click="openPanel = 'nodes'"
              title="Nodes on the mesh"
              :disabled="!connected"
            >
              ⧉ nodes
            </button>
            <button
              class="tb-btn"
              @click="openPositionModal"
              title="Share your position"
              :disabled="!connected"
            >
              📍 pos
            </button>
            <button
              class="tb-btn"
              @click="openPanel = 'stats'"
              title="Radio telemetry (battery, channel util, airtime)"
              :disabled="!connected"
            >
              📊 stats
            </button>
            <button
              class="tb-btn tb-btn-danger"
              @click="openRadioPanel"
              title="Radio config (region, preset, role) — advanced"
              :disabled="!connected"
            >
              ⚙ radio
            </button>
          </div>

          <div class="section">
            <div class="section-title">Spaces</div>
          <ul class="channel-list">
            <li
              v-for="s in allSpaces"
              :key="spaceKey(s)"
              class="channel-item"
              :class="{ active: isSameSpace(currentSpace, s) }"
              @click="switchSpace(s)"
            >
              <template v-if="s.kind === 'channel'">
                <span
                  class="priv-dot"
                  :class="channelPrivate(s.info) ? 'dot-private' : 'dot-public'"
                />
                <span class="chan-name">{{ channelName(s.info, s.idx) }}</span>
                <span class="chan-idx">#{{ s.idx }}</span>
              </template>
              <template v-else>
                <span class="priv-dot dot-private" />
                <span class="chan-name">
                  <span class="dm-icon">✉</span>
                  {{ displayName(s.peer) }}
                  <span v-if="isFavorite(s.peer)" class="dm-fav" title="Favorite">★</span>
                </span>
                <span
                  v-if="dmUnread[s.peer]"
                  class="dm-unread"
                  :title="`${dmUnread[s.peer]} unread`"
                >
                  +{{ dmUnread[s.peer] }}
                </span>
              </template>
            </li>
          </ul>
          <div class="section-hint" v-if="connected && sortedNodes.length > 0">
            Tip: open <strong>⧉ nodes</strong> to start a DM with a peer.
          </div>
          </div> <!-- /.section -->
        </template>

        <div class="spacer" />

        <div v-if="connected && historyInfo.restored > 0" class="sidebar-hint">
          {{ historyInfo.restored }} messages restored
          <span v-if="historyInfo.errors > 0" class="err-text">
            · {{ historyInfo.errors }} errors
          </span>
        </div>
      </aside>

      <!-- Main chat -->
      <section class="chat">
        <div class="chat-header">
          <div class="chat-title">
            <span
              class="priv-dot"
              :class="isPrivateChannel ? 'dot-private' : 'dot-public'"
            />
            <span v-if="isDmSpace" class="chat-dm-icon">✉</span>
            <span class="chat-name">{{ currentLabel }}</span>
            <span v-if="isChannelSpace" class="chat-meta">
              #{{ currentSpace.idx }}
            </span>
            <span v-else class="chat-meta">DM</span>
          </div>
          <div
            class="chat-privacy-tag"
            :class="isPrivateChannel ? 'tag-success' : 'tag-danger'"
          >
            {{
              isDmSpace
                ? "PRIVATE · end-to-end"
                : isPrivateChannel
                  ? "PRIVATE"
                  : "PUBLIC — anyone can read"
            }}
          </div>
        </div>

        <div v-if="searchVisible" class="search-bar">
          <span class="search-icon">🔍</span>
          <input
            ref="searchInputEl"
            v-model="searchQuery"
            type="text"
            class="search-input"
            placeholder="Filter messages in this space…"
            @keydown.esc.prevent="closeSearch"
          />
          <span class="search-count">
            {{ filteredMessages.length }} match{{ filteredMessages.length === 1 ? "" : "es" }}
          </span>
          <button
            type="button"
            class="search-close"
            title="Close (Esc)"
            @click="closeSearch"
          >
            ✕
          </button>
        </div>

        <div ref="messagesEl" class="messages">
          <transition-group name="msg">
            <div
              v-for="(m, idx) in filteredMessages"
              :key="idx"
              class="msg"
              :class="{ me: m.isMe, them: !m.isMe }"
            >
              <div class="bubble">
                <div class="meta">
                  <span class="meta-time">{{ fmtTime(m.timestamp) }}</span>
                  <span class="meta-dot">·</span>
                  <span class="meta-from">
                    {{ m.isMe ? meLabel() : displayName(m.from) }}
                  </span>
                  <span
                    v-if="m.isMe && m.sendStatus"
                    class="send-status"
                    :class="{
                      failed: m.sendStatus?.Failed,
                      delivered: m.sendStatus === 'Delivered',
                    }"
                    :title="
                      m.sendStatus === 'Sending'
                        ? 'awaiting serial write'
                        : m.sendStatus === 'Sent'
                          ? 'accepted by local radio'
                          : m.sendStatus === 'Delivered'
                            ? 'acknowledged by mesh'
                            : m.sendStatus?.Failed || ''
                    "
                  >
                    {{
                      m.sendStatus === "Sending"
                        ? "…"
                        : m.sendStatus === "Sent"
                          ? "✓"
                          : m.sendStatus === "Delivered"
                            ? "✓✓"
                            : m.sendStatus?.Failed
                              ? "✗"
                              : ""
                    }}
                  </span>
                  <span
                    v-if="!m.isMe && (m.rxRssi != null || m.rxSnr != null)"
                    class="radio"
                  >
                    <span v-if="m.rxRssi != null">{{ m.rxRssi }}dBm</span>
                    <span v-if="m.rxSnr != null">
                      {{ (m.rxSnr >= 0 ? "+" : "") + m.rxSnr.toFixed(1) }}dB
                    </span>
                  </span>
                  <button
                    type="button"
                    class="msg-forward"
                    :disabled="!connected"
                    title="Reply to this message"
                    @click="startReply(m)"
                  >
                    ↩
                  </button>
                  <button
                    type="button"
                    class="msg-forward"
                    :disabled="!connected"
                    title="Forward this message"
                    @click="openForward(m.text)"
                  >
                    ↗
                  </button>
                </div>
                <div v-if="m.replyToText" class="reply-quote">
                  {{ m.replyToText }}
                </div>
                <div class="body-text">{{ m.text }}</div>
                <a
                  v-if="!m.isMe && positionOf(m.from)"
                  class="position-pill"
                  :href="osmLink(positionOf(m.from).latitude, positionOf(m.from).longitude)"
                  target="_blank"
                  rel="noopener"
                  :title="`Last reported: ${positionOf(m.from).latitude.toFixed(5)}, ${positionOf(m.from).longitude.toFixed(5)}`"
                >
                  📍 {{ positionOf(m.from).latitude.toFixed(4) }},
                  {{ positionOf(m.from).longitude.toFixed(4) }}
                </a>
                <div
                  v-if="m.reactions && Object.keys(m.reactions).length > 0"
                  class="reaction-row"
                >
                  <span
                    v-for="(senders, emoji) in m.reactions"
                    :key="emoji"
                    class="reaction-pill"
                    :title="senders.map(displayName).join(', ')"
                  >
                    {{ emoji }}
                    <span class="reaction-count">{{ senders.length }}</span>
                  </span>
                </div>
                <div
                  v-if="canReactTo(m)"
                  class="reaction-picker"
                >
                  <button
                    v-for="e in REACTION_CHOICES"
                    :key="e"
                    type="button"
                    class="reaction-choice"
                    :title="`React with ${e}`"
                    @click="pickReaction(m, e)"
                  >
                    {{ e }}
                  </button>
                </div>
              </div>
            </div>
          </transition-group>
          <div v-if="filteredMessages.length === 0" class="empty">
            <div class="empty-icon">✉</div>
            <div v-if="isDmSpace">
              empty DM thread — type and hit Enter to send privately
            </div>
            <div v-else>no messages on this channel yet</div>
            <div class="empty-hint">connect and say hi</div>
          </div>
        </div>

        <div v-if="replyingTo" class="reply-bar">
          <div class="reply-bar-body">
            <div class="reply-bar-label">↩ Replying to {{ replyingTo.author }}</div>
            <div class="reply-bar-quote">{{ replyingTo.text }}</div>
          </div>
          <button
            type="button"
            class="reply-bar-x"
            title="Cancel reply"
            @click="cancelReply"
          >
            ✕
          </button>
        </div>
        <div class="composer">
          <div class="composer-chan">
            {{
              isDmSpace ? "DM" : "ch" + currentSpace.idx
            }}
          </div>
          <input
            v-model="input"
            type="text"
            class="composer-input"
            :placeholder="connected ? 'Type a message…' : 'Connect first…'"
            @keydown.enter="send"
            @keydown.esc="cancelReply"
            :disabled="!connected"
          />
          <button class="btn-primary" @click="send" :disabled="!connected">
            Send
          </button>
        </div>
      </section>
    </main>
  </div>
</template>

<style scoped>
/* ─── Shell ───────────────────────────────────────────────────────────── */
.shell {
  display: grid;
  grid-template-rows: auto 1fr;
  height: 100vh;
}

/* ─── Top bar ─────────────────────────────────────────────────────────── */
.topbar {
  display: flex;
  align-items: center;
  gap: 1.5rem;
  padding: 0.85rem 1.5rem;
  background: var(--bg-1);
  border-bottom: 1px solid var(--line-soft);
  box-shadow: var(--shadow-1);
  min-height: 56px;
}
.brand-block {
  display: flex;
  align-items: center;
  gap: 0.65rem;
}
.brand-logo {
  width: 32px;
  height: 32px;
  filter: drop-shadow(0 0 10px rgba(255, 210, 58, 0.35));
}
.brand-label {
  font-weight: 800;
  letter-spacing: 0.04em;
  font-size: 1rem;
}
.id-block {
  display: inline-flex;
  align-items: baseline;
  gap: 0.45rem;
  padding: 0.35rem 0.75rem;
  background: var(--bg-2);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius);
}
.id-label {
  color: var(--fg-dim);
  font-size: 0.72rem;
  text-transform: uppercase;
  letter-spacing: 0.12em;
}
.id-value {
  color: var(--info);
  font-family: var(--font-mono);
  font-size: 0.92rem;
  font-weight: 500;
}
.spacer {
  flex: 1;
}
.chips {
  display: flex;
  align-items: center;
  gap: 0.4rem;
}
.chip {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
  padding: 0.3rem 0.75rem;
  border-radius: 999px;
  font-size: 0.85rem;
  font-weight: 600;
  letter-spacing: 0.04em;
  border: 1px solid transparent;
}
.chip-success {
  background: var(--success-soft);
  color: var(--success);
  border-color: rgba(61, 220, 132, 0.3);
}
.chip-danger {
  background: var(--danger-soft);
  color: var(--danger);
  border-color: rgba(255, 93, 93, 0.3);
}
.chip-muted {
  background: var(--bg-2);
  color: var(--fg-muted);
  border-color: var(--line-soft);
}
.chip .pulse {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--fg-dim);
}
.chip-live .pulse {
  background: var(--success);
  box-shadow: 0 0 0 0 rgba(61, 220, 132, 0.5);
  animation: pulse 2s infinite;
}
@keyframes pulse {
  0% {
    box-shadow: 0 0 0 0 rgba(61, 220, 132, 0.4);
  }
  70% {
    box-shadow: 0 0 0 8px rgba(61, 220, 132, 0);
  }
  100% {
    box-shadow: 0 0 0 0 rgba(61, 220, 132, 0);
  }
}

/* ─── Body layout ─────────────────────────────────────────────────────── */
.body {
  display: grid;
  grid-template-columns: 260px 1fr;
  min-height: 0;
}

/* ─── Sidebar ─────────────────────────────────────────────────────────── */
.sidebar {
  background: var(--bg-1);
  border-right: 1px solid var(--line-soft);
  padding: 1.25rem 1rem;
  display: flex;
  flex-direction: column;
  gap: 1rem;
  min-height: 0;
  overflow-y: auto;
}
.connect-card {
  background: var(--bg-2);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius);
  padding: 0.85rem;
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
}
.connect-card h3 {
  margin: 0 0 0.3rem;
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.12em;
  color: var(--fg-dim);
}
.full {
  width: 100%;
}
.row {
  display: flex;
  gap: 0.4rem;
}
.section-title {
  text-transform: uppercase;
  letter-spacing: 0.12em;
  font-size: 0.78rem;
  font-weight: 700;
  color: var(--fg-dim);
  padding: 0 0.25rem 0.55rem;
}
.channel-list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.channel-item {
  display: flex;
  align-items: center;
  gap: 0.65rem;
  padding: 0.6rem 0.75rem;
  border-radius: var(--radius-sm);
  cursor: pointer;
  user-select: none;
  transition: background-color 120ms ease;
  font-size: 0.95rem;
}
.channel-item:hover {
  background: var(--bg-2);
}
.channel-item.active {
  background: var(--bg-3);
  border-left: 3px solid var(--accent);
  padding-left: calc(0.75rem - 3px);
}
.chan-name {
  flex: 1;
  font-weight: 500;
}
.chan-idx {
  color: var(--fg-dim);
  font-size: 0.78rem;
  font-family: var(--font-mono);
}
.priv-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex-shrink: 0;
}
.dot-private {
  background: var(--success);
  box-shadow: 0 0 6px rgba(61, 220, 132, 0.5);
}
.dot-public {
  background: var(--danger);
}
.sidebar-hint {
  padding: 0.35rem 0.5rem;
  font-size: 0.72rem;
  color: var(--fg-dim);
  border-top: 1px solid var(--line-soft);
}
.err-text {
  color: var(--danger);
}

/* ─── Chat area ───────────────────────────────────────────────────────── */
.chat {
  display: grid;
  grid-template-rows: auto 1fr auto;
  min-height: 0;
}
.chat-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  padding: 1rem 1.5rem;
  background: var(--bg-1);
  border-bottom: 1px solid var(--line-soft);
}
.chat-title {
  display: flex;
  align-items: center;
  gap: 0.65rem;
}
.chat-name {
  font-weight: 700;
  letter-spacing: 0.01em;
  font-size: 1.1rem;
}
.chat-meta {
  color: var(--fg-dim);
  font-size: 0.85rem;
}
.chat-privacy-tag {
  padding: 0.35rem 0.85rem;
  border-radius: 999px;
  font-size: 0.82rem;
  font-weight: 700;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.tag-success {
  background: var(--success-soft);
  color: var(--success);
}
.tag-danger {
  background: var(--danger-soft);
  color: var(--danger);
}

/* ─── Messages ────────────────────────────────────────────────────────── */
.messages {
  overflow-y: auto;
  padding: 1.25rem 1.5rem;
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
  scroll-behavior: smooth;
}
.msg {
  display: flex;
}
.msg.me {
  justify-content: flex-end;
}
.bubble {
  max-width: 82%;
  min-width: 140px;
  background: var(--bg-2);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius-lg);
  padding: 0.6rem 1rem;
  box-shadow: var(--shadow-1);
  position: relative;
}
.msg.me .bubble {
  background: linear-gradient(
    145deg,
    rgba(61, 220, 132, 0.16),
    rgba(61, 220, 132, 0.05)
  );
  border-color: rgba(61, 220, 132, 0.25);
  border-top-right-radius: 4px;
}
.msg.them .bubble {
  background: linear-gradient(
    145deg,
    rgba(88, 166, 255, 0.12),
    rgba(88, 166, 255, 0.04)
  );
  border-color: rgba(88, 166, 255, 0.2);
  border-top-left-radius: 4px;
}

.meta {
  display: flex;
  align-items: center;
  gap: 0.4rem;
  font-size: 0.88rem;
  color: var(--fg-dim);
  margin-bottom: 0.3rem;
}
.meta-from {
  font-weight: 600;
  color: var(--fg-muted);
}
.msg.me .meta-from {
  color: var(--success);
}
.msg.them .meta-from {
  color: var(--info);
}
.meta-dot {
  opacity: 0.5;
}
.send-status {
  margin-left: 0.2rem;
  color: var(--success);
  font-weight: 700;
}
.send-status.delivered {
  color: var(--success);
  text-shadow: 0 0 6px rgba(61, 220, 132, 0.45);
}
.send-status.failed {
  color: var(--danger);
}
.radio {
  margin-left: auto;
  font-family: ui-monospace, "SF Mono", Menlo, monospace;
  font-size: 0.7rem;
  opacity: 0.75;
}
.radio span + span {
  margin-left: 0.4rem;
}
.body-text {
  white-space: pre-wrap;
  word-wrap: break-word;
  line-height: 1.55;
  font-size: 1.15rem;
}

/* enter transition */
.msg-enter-active {
  transition: all 180ms ease;
}
.msg-enter-from {
  opacity: 0;
  transform: translateY(6px);
}

.empty {
  margin: auto;
  text-align: center;
  color: var(--fg-dim);
  padding: 2rem;
}
.empty-icon {
  font-size: 2rem;
  opacity: 0.4;
  margin-bottom: 0.5rem;
}
.empty-hint {
  font-size: 0.8rem;
  opacity: 0.6;
  margin-top: 0.25rem;
}

/* ─── Unlock modal ────────────────────────────────────────────────────── */
.unlock-overlay {
  position: fixed;
  inset: 0;
  background: rgba(4, 6, 10, 0.75);
  backdrop-filter: blur(10px);
  display: grid;
  place-items: center;
  z-index: 1000;
}
.unlock-card {
  background: var(--bg-1);
  border: 1px solid var(--line);
  border-radius: var(--radius-lg);
  padding: 1.75rem 1.9rem;
  min-width: 360px;
  max-width: 440px;
  display: flex;
  flex-direction: column;
  gap: 0.65rem;
  box-shadow: var(--shadow-2);
}
.unlock-brand {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 0.25rem;
  margin-bottom: 0.5rem;
}
.unlock-logo {
  width: 88px;
  height: 88px;
  filter: drop-shadow(0 4px 16px rgba(255, 210, 58, 0.25));
}
.unlock-wordmark {
  font-family: var(--font-mono);
  font-weight: 700;
  letter-spacing: 0.12em;
  font-size: 0.95rem;
  color: var(--accent);
  text-transform: lowercase;
}
.unlock-title {
  font-size: 1.1rem;
  font-weight: 700;
  letter-spacing: 0.01em;
  margin-bottom: 0.2rem;
  color: var(--fg);
}
.unlock-hint {
  margin: 0 0 0.4rem;
  color: var(--fg-muted);
  font-size: 0.87rem;
  line-height: 1.5;
}
.unlock-card input[type="password"] {
  padding: 0.65rem 0.9rem;
  font-size: 0.95rem;
  font-family: var(--font-mono);
  letter-spacing: 0.15em;
}
.unlock-error {
  color: var(--danger);
  font-size: 0.84rem;
  padding: 0.5rem 0.7rem;
  background: var(--danger-soft);
  border-radius: var(--radius-sm);
  border: 1px solid rgba(255, 101, 101, 0.3);
}

/* Panel toolbar */
.panel-toolbar {
  display: flex;
  gap: 0.35rem;
  margin-bottom: 0.5rem;
}
.tb-btn {
  flex: 1;
  padding: 0.6rem 0.5rem;
  background: var(--bg-2);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius-sm);
  color: var(--fg-muted);
  font-size: 0.92rem;
  font-weight: 600;
  font-family: var(--font-mono);
  letter-spacing: 0.03em;
  cursor: pointer;
  transition: all 120ms ease;
}
.tb-btn:hover:not(:disabled) {
  background: var(--bg-3);
  border-color: var(--accent);
  color: var(--fg);
}
.tb-btn-danger {
  color: var(--danger);
}
.tb-btn-danger:hover:not(:disabled) {
  border-color: var(--danger);
  color: var(--danger);
}

/* Radio config modal */
.radio-warn {
  padding: 0.7rem 0.9rem;
  border: 1px solid rgba(255, 101, 101, 0.35);
  background: var(--danger-soft);
  color: var(--danger);
  border-radius: var(--radius-sm);
  font-size: 0.88rem;
  line-height: 1.45;
}
.radio-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 0.6rem 1rem;
}
.radio-diff {
  padding: 0.7rem 0.9rem;
  background: var(--bg-2);
  border: 1px solid var(--accent);
  border-radius: var(--radius-sm);
}
.radio-diff-title {
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.1em;
  color: var(--accent);
  margin-bottom: 0.35rem;
}
.radio-diff ul {
  margin: 0;
  padding-left: 1.1rem;
  font-family: var(--font-mono);
  font-size: 0.82rem;
  color: var(--fg-muted);
}

/* Panel modal (identity / channels / nodes) */
.panel-overlay {
  position: fixed;
  inset: 0;
  background: rgba(4, 6, 10, 0.72);
  backdrop-filter: blur(8px);
  display: grid;
  place-items: center;
  z-index: 900;
  padding: 2rem;
}
.panel-card {
  background: var(--bg-1);
  border: 1px solid var(--line);
  border-radius: var(--radius-lg);
  padding: 1.25rem 1.5rem;
  min-width: 380px;
  max-width: 520px;
  width: 100%;
  display: flex;
  flex-direction: column;
  gap: 0.7rem;
  box-shadow: var(--shadow-2);
  max-height: 90vh;
  overflow-y: auto;
}
.panel-card-lg {
  max-width: 760px;
}
.panel-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  border-bottom: 1px solid var(--line-soft);
  padding-bottom: 0.55rem;
  margin-bottom: 0.3rem;
}
.panel-head h3 {
  margin: 0;
  font-size: 1.05rem;
  letter-spacing: 0.02em;
}
.panel-x {
  background: transparent;
  border: none;
  color: var(--fg-muted);
  font-size: 1.1rem;
  padding: 0 0.4rem;
  cursor: pointer;
}
.panel-x:hover {
  color: var(--fg);
  background: transparent;
  border: none;
}
.panel-hint {
  margin: 0 0 0.4rem;
  color: var(--fg-muted);
  font-size: 0.85rem;
  line-height: 1.5;
}
.panel-actions {
  display: flex;
  justify-content: flex-end;
  gap: 0.5rem;
  padding-top: 0.3rem;
  border-top: 1px solid var(--line-soft);
  margin-top: 0.3rem;
}

/* Form fields */
.field {
  display: flex;
  align-items: center;
  gap: 0.6rem;
}
.field > span:first-child {
  width: 110px;
  color: var(--fg-muted);
  font-size: 0.85rem;
}
.field input,
.field select {
  flex: 1;
}
.field-count {
  color: var(--fg-dim);
  font-size: 0.75rem;
  font-family: var(--font-mono);
  min-width: 40px;
  text-align: right;
}

/* Tables */
.chan-table,
.nodes-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.85rem;
}
.chan-table th,
.nodes-table th {
  text-align: left;
  color: var(--fg-dim);
  font-size: 0.72rem;
  text-transform: uppercase;
  letter-spacing: 0.08em;
  padding: 0.4rem 0.5rem;
  border-bottom: 1px solid var(--line-soft);
}
.chan-table td,
.nodes-table td {
  padding: 0.4rem 0.5rem;
  border-bottom: 1px solid var(--line-soft);
  vertical-align: middle;
}
.chan-table tr.disabled td {
  color: var(--fg-dim);
}
.role-pill {
  display: inline-block;
  padding: 0.1rem 0.5rem;
  border-radius: 999px;
  font-size: 0.7rem;
  background: var(--bg-3);
  color: var(--fg-muted);
}
.role-pill.primary {
  background: var(--success-soft);
  color: var(--success);
}
.role-pill.secondary {
  background: var(--info-soft);
  color: var(--info);
}
.tag-success-sm,
.tag-danger-sm {
  display: inline-block;
  padding: 0.05rem 0.4rem;
  border-radius: 999px;
  font-size: 0.68rem;
  font-weight: 700;
  letter-spacing: 0.05em;
  margin-right: 0.4rem;
}
.tag-success-sm {
  background: var(--success-soft);
  color: var(--success);
}
.tag-danger-sm {
  background: var(--danger-soft);
  color: var(--danger);
}
.psk-preview {
  color: var(--fg-dim);
  font-family: var(--font-mono);
  font-size: 0.75rem;
}
.chan-actions {
  display: flex;
  gap: 0.3rem;
  justify-content: flex-end;
}
.chan-actions button {
  padding: 0.3rem 0.6rem;
  font-size: 0.8rem;
}
.chan-actions-col {
  width: 160px;
}
.btn-danger {
  background: var(--danger-soft);
  color: var(--danger);
  border-color: rgba(255, 101, 101, 0.3);
}
.btn-danger:hover:not(:disabled) {
  background: var(--danger);
  color: #fff;
}
.btn-primary.sm {
  padding: 0.25rem 0.55rem;
  font-size: 0.78rem;
}
.chan-edit {
  border-top: 1px solid var(--line-soft);
  padding-top: 0.8rem;
  margin-top: 0.3rem;
  display: flex;
  flex-direction: column;
  gap: 0.55rem;
}
.chan-edit h4 {
  margin: 0;
  font-size: 0.9rem;
  letter-spacing: 0.02em;
  color: var(--accent);
}
.mono {
  font-family: var(--font-mono);
  font-size: 0.8rem;
  color: var(--info);
}
.self-badge {
  color: var(--success);
  margin-right: 0.2rem;
}
.short {
  color: var(--fg-dim);
  font-size: 0.78rem;
  margin-left: 0.3rem;
}
.empty-row {
  text-align: center;
  color: var(--fg-dim);
  padding: 1.5rem 0;
}

/* Inline position indicator on received bubbles */
.position-pill {
  display: inline-block;
  margin-top: 0.4rem;
  padding: 0.2rem 0.55rem;
  background: var(--bg-2);
  border: 1px solid var(--line-soft);
  border-radius: 999px;
  color: var(--fg-muted);
  font-size: 0.78rem;
  font-family: var(--font-mono);
  text-decoration: none;
  transition: color 120ms ease, border-color 120ms ease;
}
.position-pill:hover {
  color: var(--accent);
  border-color: var(--accent);
}
.position-link {
  color: var(--info);
  text-decoration: none;
  font-size: 1rem;
}
.position-link:hover {
  color: var(--accent);
}

/* In-space search bar (Ctrl+F) */
.search-bar {
  display: flex;
  align-items: center;
  gap: 0.55rem;
  padding: 0.45rem 1.5rem;
  background: var(--bg-2);
  border-bottom: 1px solid var(--line-soft);
}
.search-icon {
  color: var(--fg-dim);
  font-size: 0.95rem;
}
.search-input {
  flex: 1;
  padding: 0.4rem 0.65rem;
  background: var(--bg-1);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius-sm);
  color: var(--fg);
  font-size: 0.9rem;
}
.search-input:focus {
  outline: none;
  border-color: var(--accent);
}
.search-count {
  color: var(--fg-dim);
  font-size: 0.78rem;
  font-family: var(--font-mono);
  white-space: nowrap;
}
.search-close {
  background: transparent;
  border: none;
  color: var(--fg-dim);
  font-size: 0.9rem;
  padding: 0 0.4rem;
  cursor: pointer;
}
.search-close:hover {
  color: var(--fg);
  background: transparent;
  border: none;
}

/* Emoji reactions (native Meshtastic feature) */
.reaction-row {
  display: flex;
  flex-wrap: wrap;
  gap: 0.3rem;
  margin-top: 0.4rem;
}
.reaction-pill {
  display: inline-flex;
  align-items: center;
  gap: 0.25rem;
  padding: 0.15rem 0.5rem;
  background: var(--bg-2);
  border: 1px solid var(--line-soft);
  border-radius: 999px;
  font-size: 0.85rem;
  line-height: 1.1;
  user-select: none;
}
.reaction-count {
  color: var(--fg-dim);
  font-size: 0.72rem;
  font-family: var(--font-mono);
}
.reaction-picker {
  display: flex;
  flex-wrap: wrap;
  gap: 0.2rem;
  margin-top: 0.5rem;
  padding: 0.35rem;
  background: var(--bg-1);
  border: 1px solid var(--line);
  border-radius: var(--radius-sm);
  box-shadow: var(--shadow-1);
}
.reaction-choice {
  background: transparent;
  border: none;
  color: var(--fg);
  font-size: 1.15rem;
  padding: 0.15rem 0.35rem;
  cursor: pointer;
  border-radius: var(--radius-sm);
  transition: background-color 120ms ease, transform 120ms ease;
}
.reaction-choice:hover {
  background: var(--bg-3);
  transform: scale(1.15);
  border: none;
}

/* Reply quote inside a bubble (the message this one replies to) */
.reply-quote {
  margin: 0 0 0.4rem;
  padding: 0.35rem 0.65rem;
  border-left: 3px solid var(--accent);
  background: rgba(255, 210, 58, 0.08);
  color: var(--fg-muted);
  font-size: 0.85rem;
  line-height: 1.35;
  border-radius: 0 var(--radius-sm) var(--radius-sm) 0;
  white-space: pre-wrap;
  word-wrap: break-word;
}

/* Compose-time "replying to" preview */
.reply-bar {
  display: flex;
  align-items: flex-start;
  gap: 0.6rem;
  padding: 0.55rem 1.5rem;
  background: var(--bg-2);
  border-top: 1px solid var(--line-soft);
}
.reply-bar-body {
  flex: 1;
  min-width: 0;
}
.reply-bar-label {
  color: var(--accent);
  font-size: 0.75rem;
  font-weight: 600;
  letter-spacing: 0.02em;
  margin-bottom: 0.15rem;
}
.reply-bar-quote {
  color: var(--fg-muted);
  font-size: 0.85rem;
  line-height: 1.35;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.reply-bar-x {
  background: transparent;
  border: none;
  color: var(--fg-dim);
  font-size: 0.95rem;
  cursor: pointer;
  padding: 0 0.3rem;
}
.reply-bar-x:hover {
  color: var(--fg);
  background: transparent;
  border: none;
}

/* Message action buttons (forward, etc.) */
.msg-forward {
  background: transparent;
  border: none;
  color: var(--fg-dim);
  font-size: 0.95rem;
  padding: 0 0.25rem;
  cursor: pointer;
  opacity: 0.55;
  transition: opacity 120ms ease, color 120ms ease, transform 120ms ease;
}
.msg-forward:hover:not(:disabled) {
  opacity: 1;
  color: var(--accent);
  transform: translateX(1px);
  background: transparent;
  border: none;
}
.msg-forward:disabled {
  cursor: not-allowed;
  opacity: 0.3;
}

/* Forward modal */
.forward-preview {
  padding: 0.65rem 0.85rem;
  background: var(--bg-2);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius-sm);
  color: var(--fg);
  font-size: 0.92rem;
  line-height: 1.45;
  white-space: pre-wrap;
  word-wrap: break-word;
  max-height: 180px;
  overflow-y: auto;
}
.forward-list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
  max-height: 320px;
  overflow-y: auto;
}
.forward-item {
  display: flex;
  align-items: center;
  gap: 0.55rem;
  padding: 0.55rem 0.7rem;
  border-radius: var(--radius-sm);
  cursor: pointer;
  user-select: none;
  transition: background-color 120ms ease;
  font-size: 0.95rem;
}
.forward-item:hover:not(.disabled) {
  background: var(--bg-3);
}
.forward-item.disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.forward-meta {
  margin-left: auto;
  color: var(--fg-dim);
  font-size: 0.78rem;
  font-family: var(--font-mono);
}

/* Favorite / alias controls */
.fav-btn {
  background: transparent;
  border: none;
  color: var(--fg-dim);
  font-size: 1.05rem;
  cursor: pointer;
  padding: 0.1rem 0.3rem;
  transition: color 120ms ease, transform 120ms ease;
}
.fav-btn:hover {
  color: var(--accent);
  background: transparent;
  border: none;
  transform: scale(1.15);
}
.fav-btn.active {
  color: var(--accent);
}
.alias-input {
  width: 100%;
  font-size: 0.8rem;
  padding: 0.2rem 0.4rem;
  background: var(--bg-2);
  border: 1px solid var(--line-soft);
  border-radius: var(--radius-sm);
  color: var(--fg);
}
.alias-input:focus {
  outline: none;
  border-color: var(--accent);
}
.dm-fav {
  color: var(--accent);
  margin-left: 0.25rem;
  font-size: 0.85em;
}

/* DM sidebar items */
.dm-icon {
  color: var(--info);
  font-weight: 600;
  margin-right: 0.2rem;
}
.dm-unread {
  margin-left: auto;
  background: var(--accent);
  color: #111;
  padding: 0 0.4rem;
  border-radius: 999px;
  font-size: 0.7rem;
  font-weight: 700;
}
.chat-dm-icon {
  color: var(--info);
  margin-right: 0.1rem;
}
.section-hint {
  padding: 0.5rem 0.25rem 0;
  color: var(--fg-dim);
  font-size: 0.75rem;
  border-top: 1px solid var(--line-soft);
  margin-top: 0.4rem;
}

/* ─── Composer ────────────────────────────────────────────────────────── */
.composer {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  padding: 1rem 1.5rem;
  background: var(--bg-1);
  border-top: 1px solid var(--line-soft);
}
.composer-chan {
  font-family: var(--font-mono);
  font-size: 0.85rem;
  font-weight: 600;
  color: var(--accent);
  padding: 0.6rem 0.85rem;
  background: var(--bg-2);
  border-radius: var(--radius-sm);
  border: 1px solid var(--line-soft);
  min-width: 56px;
  text-align: center;
}
.composer-input {
  flex: 1;
  padding: 0.7rem 1rem;
  font-size: 1rem;
}
.composer .btn-primary {
  padding: 0.7rem 1.4rem;
  font-size: 0.95rem;
}
</style>
