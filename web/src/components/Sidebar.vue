<script setup lang="ts">
import { computed, ref } from 'vue';
import { engine } from '../engine';
import { catalog, ensureRange, segmentsIn, ui, type CatalogDay } from '../state';
import { clamp, fmtDur, MIN } from '../util';
import ChannelTree from './ChannelTree.vue';
import SpeakerList from './SpeakerList.vue';
import VirtualList from './VirtualList.vue';

const days = computed(() => [...(catalog.value?.days ?? [])].reverse());
const daysOpen = ref(true);

async function openDay(d: CatalogDay) {
  await ensureRange(d.from, d.to);
  const segs = segmentsIn(d.from, d.to);
  const a = segs.length ? segs[0].startMs : d.from;
  const b = segs.length ? Math.max(...segs.map((s) => s.endMs)) : d.to;
  const pad = Math.max(MIN, (b - a) * 0.03);
  ui.view.msPerPx = (b - a + 2 * pad) / ui.view.width;
  ui.view.start = a - pad;
  ui.scrollY = 0;
  engine.seek(a);
}

function startResize(e: PointerEvent) {
  const el = e.currentTarget as HTMLElement;
  el.setPointerCapture(e.pointerId);
  const x0 = e.clientX;
  const w0 = ui.sidebarWidth;
  const move = (ev: PointerEvent) => (ui.sidebarWidth = clamp(w0 + ev.clientX - x0, 200, 700));
  const up = () => {
    el.removeEventListener('pointermove', move);
    el.removeEventListener('pointerup', up);
  };
  el.addEventListener('pointermove', move);
  el.addEventListener('pointerup', up);
}
</script>

<template>
  <aside class="sidebar" :style="{ width: `${ui.sidebarWidth}px` }">
    <ChannelTree />
    <SpeakerList />
    <section class="days" :class="{ open: daysOpen }">
      <header @click="daysOpen = !daysOpen">
        <h3>{{ daysOpen ? '▾' : '▸' }} Days <span class="dim">{{ days.length }}</span></h3>
      </header>
      <VirtualList v-if="daysOpen" :items="days" :row-height="22" :key-of="(d: CatalogDay) => d.day">
        <template #default="{ item }">
          <div class="day" @click="openDay(item)">
            <span>{{ item.day }}</span>
            <span class="dim">{{ item.totalMs === null ? '' : fmtDur(item.totalMs) }}</span>
          </div>
        </template>
      </VirtualList>
    </section>
    <div class="resizer" @pointerdown="startResize" title="Drag to resize" />
  </aside>
</template>

<style scoped>
.sidebar {
  position: relative;
  flex: none;
  display: flex;
  flex-direction: column;
  border-right: 1px solid var(--line);
  background: var(--panel);
  min-height: 0;
  font-size: 13px;
}
.days {
  border-top: 1px solid var(--line);
  display: flex;
  flex-direction: column;
  min-height: 0;
  padding-top: 6px;
}
.days.open {
  flex: 0 1 180px;
}
header {
  padding: 0 10px 4px;
  cursor: pointer;
}
h3 {
  margin: 0;
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.06em;
  color: var(--dim);
}
.day {
  display: flex;
  justify-content: space-between;
  align-items: center;
  height: 22px;
  padding: 0 10px;
  cursor: pointer;
  font-variant-numeric: tabular-nums;
}
.day:hover {
  background: rgba(255, 255, 255, 0.04);
}
.dim {
  color: var(--dim);
  font-weight: 400;
}
.resizer {
  position: absolute;
  top: 0;
  right: -3px;
  width: 6px;
  height: 100%;
  cursor: col-resize;
  z-index: 5;
}
.resizer:hover {
  background: rgba(122, 162, 255, 0.35);
}
</style>
