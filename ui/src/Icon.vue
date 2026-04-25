<script setup>
defineProps({
  name: { type: String, required: true },
  size: { type: [Number, String], default: 16 },
});
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.8"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
    class="icon"
    :class="`icon-${name}`"
  >
    <template v-if="name === 'close'">
      <line x1="6" y1="6" x2="18" y2="18" />
      <line x1="18" y1="6" x2="6" y2="18" />
    </template>

    <template v-else-if="name === 'search'">
      <circle cx="10.5" cy="10.5" r="6.5" />
      <line x1="15.5" y1="15.5" x2="20" y2="20" />
    </template>

    <template v-else-if="name === 'refresh'">
      <path d="M21 12a9 9 0 0 1 -15.5 6.5L3 16" />
      <polyline points="3 21 3 16 8 16" />
      <path d="M3 12a9 9 0 0 1 15.5 -6.5L21 8" />
      <polyline points="21 3 21 8 16 8" />
    </template>

    <template v-else-if="name === 'mail'">
      <rect x="3" y="5.5" width="18" height="13" rx="2" />
      <path d="M3 7l9 7 9 -7" />
    </template>

    <template v-else-if="name === 'gear'">
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06 .06a2 2 0 1 1 -2.83 2.83l-.06 -.06a1.65 1.65 0 0 0 -1.82 -.33 1.65 1.65 0 0 0 -1 1.51V21a2 2 0 1 1 -4 0v-.09a1.65 1.65 0 0 0 -1 -1.51 1.65 1.65 0 0 0 -1.82 .33l-.06 .06a2 2 0 1 1 -2.83 -2.83l.06 -.06a1.65 1.65 0 0 0 .33 -1.82 1.65 1.65 0 0 0 -1.51 -1H3a2 2 0 1 1 0 -4h.09a1.65 1.65 0 0 0 1.51 -1 1.65 1.65 0 0 0 -.33 -1.82l-.06 -.06a2 2 0 1 1 2.83 -2.83l.06 .06a1.65 1.65 0 0 0 1.82 .33H9a1.65 1.65 0 0 0 1 -1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82 -.33l.06 -.06a2 2 0 1 1 2.83 2.83l-.06 .06a1.65 1.65 0 0 0 -.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0 -1.51 1z" />
    </template>

    <template v-else-if="name === 'trash'">
      <polyline points="3 6 5 6 21 6" />
      <path d="M19 6l-1 14a2 2 0 0 1 -2 2H8a2 2 0 0 1 -2 -2L5 6" />
      <path d="M10 11v6" />
      <path d="M14 11v6" />
      <path d="M9 6V4a2 2 0 0 1 2 -2h2a2 2 0 0 1 2 2v2" />
    </template>

    <template v-else-if="name === 'lock-closed'">
      <rect x="5" y="11" width="14" height="10" rx="2" />
      <path d="M8 11V7a4 4 0 0 1 8 0v4" />
    </template>

    <template v-else-if="name === 'lock-open'">
      <rect x="5" y="11" width="14" height="10" rx="2" />
      <path d="M8 11V7a4 4 0 0 1 7.5 -2" />
    </template>

    <template v-else-if="name === 'door-out'">
      <path d="M9 21H5a2 2 0 0 1 -2 -2V5a2 2 0 0 1 2 -2h4" />
      <polyline points="16 17 21 12 16 7" />
      <line x1="21" y1="12" x2="9" y2="12" />
    </template>

    <template v-else-if="name === 'megaphone'">
      <path d="M3 11v3a2 2 0 0 0 2 2h2l4 4V5l-4 4H5a2 2 0 0 0 -2 2z" />
      <path d="M15 7a5 5 0 0 1 0 10" />
      <path d="M18 4a9 9 0 0 1 0 16" />
    </template>

    <template v-else-if="name === 'pin'">
      <path d="M21 10c0 7 -9 13 -9 13S3 17 3 10a9 9 0 0 1 18 0z" />
      <circle cx="12" cy="10" r="3" />
    </template>

    <template v-else-if="name === 'radar'">
      <circle cx="12" cy="12" r="2" />
      <path d="M8 12a4 4 0 0 1 8 0" />
      <path d="M5 12a7 7 0 0 1 14 0" />
      <path d="M12 12L20 5" />
    </template>

    <template v-else-if="name === 'mesh-dots'">
      <circle cx="6" cy="6" r="2" />
      <circle cx="18" cy="6" r="2" />
      <circle cx="6" cy="18" r="2" />
      <circle cx="18" cy="18" r="2" />
      <circle cx="12" cy="12" r="2" />
      <line x1="6" y1="6" x2="12" y2="12" />
      <line x1="18" y1="6" x2="12" y2="12" />
      <line x1="6" y1="18" x2="12" y2="12" />
      <line x1="18" y1="18" x2="12" y2="12" />
    </template>

    <template v-else-if="name === 'tower'">
      <line x1="12" y1="22" x2="12" y2="10" />
      <path d="M5 22L12 6L19 22" />
      <path d="M7.5 16h9" />
      <circle cx="12" cy="6" r="2" />
    </template>

    <template v-else-if="name === 'home'">
      <path d="M3 11l9 -8l9 8" />
      <path d="M5 10v10a1 1 0 0 0 1 1h12a1 1 0 0 0 1 -1v-10" />
      <line x1="10" y1="21" x2="10" y2="14" />
      <line x1="14" y1="21" x2="14" y2="14" />
    </template>

    <template v-else-if="name === 'bolt'">
      <polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2" />
    </template>

    <template v-else-if="name === 'star-filled'">
      <polygon
        points="12 2 15 9 22 9.5 17 14.5 18.5 22 12 18 5.5 22 7 14.5 2 9.5 9 9 12 2"
        fill="currentColor"
        stroke="currentColor"
      />
    </template>

    <template v-else-if="name === 'star'">
      <polygon
        points="12 2 15 9 22 9.5 17 14.5 18.5 22 12 18 5.5 22 7 14.5 2 9.5 9 9 12 2"
      />
    </template>

    <template v-else-if="name === 'reply'">
      <polyline points="9 17 4 12 9 7" />
      <path d="M20 18v-2a4 4 0 0 0 -4 -4H4" />
    </template>

    <template v-else-if="name === 'forward'">
      <polyline points="15 17 20 12 15 7" />
      <path d="M4 18v-2a4 4 0 0 1 4 -4h12" />
    </template>

    <template v-else-if="name === 'info'">
      <circle cx="12" cy="12" r="9" />
      <line x1="12" y1="11" x2="12" y2="16" />
      <circle cx="12" cy="8" r="0.8" fill="currentColor" />
    </template>

    <template v-else-if="name === 'warning'">
      <path d="M12 3l10 17H2L12 3z" />
      <line x1="12" y1="9" x2="12" y2="14" />
      <circle cx="12" cy="17.5" r="0.8" fill="currentColor" />
    </template>

    <template v-else-if="name === 'connect'">
      <path d="M16 8a5 5 0 0 0 -7 0L5 12" />
      <path d="M8 16a5 5 0 0 0 7 0L19 12" />
    </template>

    <template v-else-if="name === 'disconnect'">
      <path d="M9 12L5 8" />
      <path d="M15 12L19 16" />
      <path d="M16 8a5 5 0 0 0 -7 0" />
      <path d="M8 16a5 5 0 0 0 7 0" />
      <line x1="3" y1="3" x2="21" y2="21" />
    </template>

    <template v-else-if="name === 'plug'">
      <path d="M9 2v6" />
      <path d="M15 2v6" />
      <rect x="7" y="8" width="10" height="6" rx="1" />
      <path d="M12 14v3a4 4 0 0 0 4 4" />
    </template>

    <template v-else-if="name === 'send'">
      <line x1="22" y1="2" x2="11" y2="13" />
      <polygon points="22 2 15 22 11 13 2 9 22 2" />
    </template>

    <template v-else-if="name === 'check'">
      <polyline points="20 6 9 17 4 12" />
    </template>

    <template v-else-if="name === 'hourglass'">
      <path d="M6 2h12" />
      <path d="M6 22h12" />
      <path d="M6 2v4l6 6 6 -6V2" />
      <path d="M6 22v-4l6 -6 6 6v4" />
    </template>

    <template v-else-if="name === 'pencil'">
      <path d="M12 20h9" />
      <path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1 -4L16.5 3.5z" />
    </template>

    <template v-else-if="name === 'wipe'">
      <path d="M3 6h18" />
      <path d="M19 6l-1 14a2 2 0 0 1 -2 2H8a2 2 0 0 1 -2 -2L5 6" />
      <path d="M10 11l4 4M14 11l-4 4" />
    </template>

    <template v-else-if="name === 'reaction'">
      <circle cx="12" cy="12" r="9" />
      <path d="M8 14a4 4 0 0 0 8 0" />
      <line x1="9" y1="9" x2="9" y2="9.5" />
      <line x1="15" y1="9" x2="15" y2="9.5" />
    </template>

    <template v-else-if="name === 'list'">
      <line x1="8" y1="6" x2="21" y2="6" />
      <line x1="8" y1="12" x2="21" y2="12" />
      <line x1="8" y1="18" x2="21" y2="18" />
      <circle cx="4" cy="6" r="1.2" fill="currentColor" stroke="none" />
      <circle cx="4" cy="12" r="1.2" fill="currentColor" stroke="none" />
      <circle cx="4" cy="18" r="1.2" fill="currentColor" stroke="none" />
    </template>

    <template v-else-if="name === 'chart'">
      <line x1="3" y1="20" x2="21" y2="20" />
      <line x1="6" y1="20" x2="6" y2="14" />
      <line x1="11" y1="20" x2="11" y2="9" />
      <line x1="16" y1="20" x2="16" y2="12" />
      <line x1="21" y1="20" x2="21" y2="6" />
    </template>

    <template v-else-if="name === 'wifi'">
      <path d="M3 9a14 14 0 0 1 18 0" />
      <path d="M6 12a10 10 0 0 1 12 0" />
      <path d="M9 15a6 6 0 0 1 6 0" />
      <circle cx="12" cy="18" r="0.8" fill="currentColor" stroke="none" />
    </template>
  </svg>
</template>

<style scoped>
.icon {
  display: inline-block;
  vertical-align: -0.18em;
  flex-shrink: 0;
}

/* Per-icon accent colors. Each icon uses currentColor on its strokes
 * (set by the shared <svg> attribute) so a single class flips both
 * stroke and any fill="currentColor" inner shapes. Place these last
 * so the cascading is predictable. */
.icon-close { color: var(--fg-dim); }
.icon-search { color: var(--fg-dim); }
.icon-refresh { color: var(--success); }
.icon-mail { color: var(--info); }
.icon-gear { color: var(--fg-muted); }
.icon-trash { color: var(--danger); }
.icon-wipe { color: var(--danger); }
.icon-lock-closed { color: var(--accent); }
.icon-lock-open { color: var(--success); }
.icon-door-out { color: var(--fg-muted); }
.icon-megaphone { color: #ff9b3a; }
.icon-pin { color: var(--danger); }
.icon-radar { color: var(--info); }
.icon-mesh-dots { color: #b07cff; }
.icon-tower { color: var(--info); }
.icon-home { color: var(--accent); }
.icon-bolt { color: var(--accent); }
.icon-star-filled { color: var(--accent); }
.icon-star { color: var(--fg-dim); }
.icon-reply { color: var(--fg-muted); }
.icon-forward { color: var(--fg-muted); }
.icon-info { color: var(--info); }
.icon-warning { color: var(--accent); }
.icon-connect { color: var(--success); }
.icon-disconnect { color: var(--danger); }
.icon-plug { color: var(--success); }
.icon-send { color: var(--accent); }
.icon-check { color: var(--success); }
.icon-hourglass { color: var(--fg-muted); }
.icon-pencil { color: var(--info); }
.icon-reaction { color: var(--accent); }
.icon-list { color: var(--fg-muted); }
.icon-chart { color: var(--info); }
.icon-wifi { color: var(--info); }
</style>
