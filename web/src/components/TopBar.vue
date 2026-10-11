<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { catalog, refreshCatalog, ui } from '../state';
import { fmtBytes, fmtDateTime, fmtDur, startOfDay, DAY } from '../util';
import { engine } from '../engine';

const stats = computed(() => catalog.value?.stats);
const ratio = computed(() => {
  const s = stats.value;
  if (!s || !s.bytes) return '';
  const kbps = (s.bytes * 8) / Math.max(1, s.totalMs);
  return `${kbps.toFixed(1)} kbps avg`;
});

const date = ref('');
watch(
  () => ui.view.start,
  (t) => {
    const d = new Date(t + (ui.view.width * ui.view.msPerPx) / 2);
    date.value = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  },
  { immediate: true },
);

function goDate() {
  const [y, m, d] = date.value.split('-').map(Number);
  if (!y) return;
  const t = new Date(y, m - 1, d).getTime();
  ui.view.msPerPx = DAY / ui.view.width;
  ui.view.start = startOfDay(t);
}

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
      <span v-if="stats.pending" class="warn">{{ stats.pending }} processing</span>
    </div>
    <div class="spacer" />
    <input type="date" v-model="date" @change="goDate" title="Go to day" />
    <button @click="latest" title="Jump to the most recent speech">Latest</button>
    <button @click="refreshCatalog" :disabled="ui.loading" :title="catalog ? `index generated ${fmtDateTime(catalog.generatedAt)}` : ''">
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
.error {
  padding: 8px 14px;
  background: #4a1d1f;
  color: #ffb4b0;
  font-size: 13px;
}
</style>
