<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { catalog, refreshCatalog, ui } from '../state';
import { addDays, browserZone, fmtBytes, fmtDate, fmtDateTime, fmtDur, setTimeZone, timeZone, zonedTime, zoneNames } from '../util';
import { engine } from '../engine';

const stats = computed(() => catalog.value?.stats);
const ratio = computed(() => {
  const s = stats.value;
  if (!s || !s.bytes) return '';
  const kbps = (s.bytes * 8) / Math.max(1, s.totalMs);
  return `${kbps.toFixed(1)} kbps avg`;
});

/** Day shown in the date picker: the day at the middle of the view (display zone). */
const date = ref('');
watch(
  () => [ui.view.start, timeZone.value],
  () => (date.value = fmtDate(ui.view.start + (ui.view.width * ui.view.msPerPx) / 2)),
  { immediate: true },
);

/** Step the date picker by whole days and show that day. */
function shiftDay(n: number) {
  const [y, m, d] = date.value.split('-').map(Number);
  if (!y) return;
  // calendar arithmetic only (no zone involved)
  date.value = new Date(Date.UTC(y, m - 1, d + n)).toISOString().slice(0, 10);
  goDate();
}

function goDate() {
  const [y, m, d] = date.value.split('-').map(Number);
  if (!y) return;
  const t = zonedTime(y, m, d);
  ui.view.msPerPx = (addDays(t, 1) - t) / ui.view.width;
  ui.view.start = t;
}

const zones = zoneNames();
const zone = computed({
  get: () => timeZone.value,
  set: (v: string) => setTimeZone(v),
});

function latest() {
  const t = stats.value?.latestMs;
  if (!t) return;
  const span = 10 * 60_000;
  ui.view.msPerPx = span / ui.view.width;
  ui.view.start = t - span * 0.85;
  engine.seek(t - span * 0.5);
}
</script>

<template>
  <div class="topbar">
    <div class="brand">
      <svg viewBox="0 0 32 32" width="20" height="20"><g fill="#e5484d"><rect x="5" y="13" width="3" height="6" rx="1.5"/><rect x="10" y="9" width="3" height="14" rx="1.5"/><rect x="15" y="5" width="3" height="22" rx="1.5"/><rect x="20" y="10" width="3" height="12" rx="1.5"/><rect x="25" y="14" width="3" height="4" rx="1.5"/></g></svg>
      <span>Mumble Archive</span>
    </div>
    <div class="server" v-if="catalog">
      {{ catalog.server }} · {{ catalog.bot }}
    </div>
    <div class="stats" v-if="stats">
      <span>{{ fmtDur(stats.totalMs) }} speech</span>
      <span>{{ stats.segments.toLocaleString() }} clips</span>
      <span :title="`original stream ${fmtBytes(stats.rawBytes)}`">{{ fmtBytes(stats.bytes) }} stored · {{ ratio }}</span>
    </div>
    <div class="spacer" />
    <select v-model="zone" class="zone" title="Time zone for dates and clocks">
      <option value="">Browser ({{ browserZone }})</option>
      <option value="UTC">UTC</option>
      <option v-for="z in zones" :key="z" :value="z">{{ z }}</option>
    </select>
    <div class="datenav">
      <button @click="shiftDay(-1)" title="Previous day">‹</button>
      <input type="date" v-model="date" @change="goDate" title="Go to day" />
      <button @click="shiftDay(1)" title="Next day">›</button>
    </div>
    <button @click="latest" title="Jump to the most recent speech">Latest</button>
    <button @click="refreshCatalog()" :disabled="ui.loading" :title="catalog ? `updated ${fmtDateTime(catalog.generatedAt)}` : ''">
      {{ ui.loading ? 'Loading…' : 'Refresh' }}
    </button>
  </div>
  <div v-if="ui.error" class="error">{{ ui.error }}</div>
</template>

<style scoped>
.topbar {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 8px 14px;
  border-bottom: 1px solid var(--line);
  background: var(--panel);
  font-size: 13px;
}
.brand {
  display: flex;
  align-items: center;
  gap: 8px;
  font-weight: 700;
  font-size: 14px;
}
.server {
  color: var(--dim);
}
.stats {
  display: flex;
  gap: 14px;
  color: var(--dim);
  font-variant-numeric: tabular-nums;
}
.warn {
  color: #d4a72c;
}
.spacer {
  flex: 1;
}
.zone {
  max-width: 190px;
}
.datenav {
  display: flex;
  gap: 2px;
}
.datenav button {
  padding-inline: 8px;
}
.error {
  padding: 8px 14px;
  background: #4a1d1f;
  color: #ffb4b0;
  font-size: 13px;
}
</style>
