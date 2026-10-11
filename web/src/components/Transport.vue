<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue';
import { engine } from '../engine';
import { isAudible, nextSegment, prevSegment, ui } from '../state';
import { clamp, fmtClock, fmtDate, fmtDur, startOfDay, DAY } from '../util';

const level = ref(0);
let raf = 0;
const loop = () => {
  raf = requestAnimationFrame(loop);
  level.value = Math.max(engine.level(), level.value * 0.9);
};
onMounted(() => (raf = requestAnimationFrame(loop)));
onBeforeUnmount(() => cancelAnimationFrame(raf));

function reveal(t: number) {
  const span = ui.view.width * ui.view.msPerPx;
  if (t < ui.view.start || t > ui.view.start + span) ui.view.start = t - span * 0.2;
}

async function jump(dir: 1 | -1) {
  const t = engine.now();
  const s = dir > 0 ? await nextSegment(t + 50, isAudible) : await prevSegment(t, isAudible);
  if (s) {
    engine.seek(s.startMs);
    reveal(s.startMs);
  }
}

function stop() {
  engine.pause();
  if (ui.selection) engine.seek(ui.selection.from);
}


function zoom(f: number) {
  const span = ui.view.width * ui.view.msPerPx;
  const center = ui.playhead >= ui.view.start && ui.playhead <= ui.view.start + span ? ui.playhead : ui.view.start + span / 2;
  const x = (center - ui.view.start) / ui.view.msPerPx;
  ui.view.msPerPx = clamp(ui.view.msPerPx * f, 0.05, (14 * DAY) / 800);
  ui.view.start = center - x * ui.view.msPerPx;
}

function fitSelection() {
  const s = ui.selection;
  if (!s) return;
  const pad = (s.to - s.from) * 0.05;
  ui.view.msPerPx = (s.to - s.from + 2 * pad) / ui.view.width;
  ui.view.start = s.from - pad;
}

function fitDay() {
  const d = startOfDay(ui.playhead);
  ui.view.msPerPx = DAY / ui.view.width;
  ui.view.start = d;
}

defineExpose({ jump, zoom, fitSelection, stop });
</script>

<template>
  <footer class="transport">
    <div class="buttons">
      <button @click="jump(-1)" title="Previous speech  [">⏮</button>
      <button class="play" @click="engine.toggle()" :title="ui.playing ? 'Pause (Space)' : 'Play (Space)'">{{ ui.playing ? '❚❚' : '▶' }}</button>
      <button @click="stop" title="Stop">■</button>
      <button @click="jump(1)" title="Next speech  ]">⏭</button>
    </div>
    <div class="time">
      <div class="clock">{{ fmtClock(ui.playhead, true) }}</div>
      <div class="date">{{ fmtDate(ui.playhead) }}</div>
    </div>
    <div class="sel" v-if="ui.selection">
      <div>{{ fmtClock(ui.selection.from, true) }} → {{ fmtClock(ui.selection.to, true) }}</div>
      <div class="dim">selection · {{ fmtDur(ui.selection.to - ui.selection.from) }}</div>
    </div>
    <div class="sel dim" v-else>drag on the timeline to select a range</div>
    <div class="toggles">
      <label title="Jump over stretches where nobody (audible) speaks"><input type="checkbox" v-model="ui.skipSilence" /> Skip silence</label>
      <label title="Keep the playhead in view"><input type="checkbox" v-model="ui.follow" /> Follow</label>
      <label title="Loop the selection (L)"><input type="checkbox" v-model="ui.loop" /> Loop</label>
    </div>
    <button
      class="wheel"
      @click="ui.wheelMode = ui.wheelMode === 'zoom' ? 'scroll' : 'zoom'"
      :title="'Mouse wheel over the tracks ' + (ui.wheelMode === 'zoom' ? 'zooms' : 'scrolls') + ' (W to toggle); Shift+wheel does the other. Wheel over the ruler always zooms, over the overview always scrolls.'"
    >Wheel: {{ ui.wheelMode === 'zoom' ? 'Zoom' : 'Scroll' }}</button>
    <div class="zoom">
      <button @click="zoom(1 / 1.6)" title="Zoom in (+ / Ctrl+wheel)">+</button>
      <button @click="zoom(1.6)" title="Zoom out (-)">−</button>
      <button @click="fitSelection" :disabled="!ui.selection" title="Zoom to selection (Z)">Sel</button>
      <button @click="fitDay" title="Whole day">Day</button>
    </div>
    <div class="master">
      <input type="range" min="0" max="2" step="0.01" v-model.number="ui.mix.master" @input="engine.applyGains()" title="Master volume" />
      <div class="meter"><div :style="{ width: `${Math.min(100, Math.sqrt(level) * 100)}%` }" /></div>
    </div>
    <button class="primary" @click="ui.exportOpen = true" title="Download a range (E)">Export…</button>
  </footer>
</template>

<style scoped>
.transport {
  display: flex;
  align-items: center;
  gap: 18px;
  padding: 8px 14px;
  border-top: 1px solid var(--line);
  background: var(--panel);
  font-size: 13px;
}
.buttons {
  display: flex;
  gap: 4px;
}
.buttons button {
  width: 34px;
  height: 30px;
  font-size: 14px;
}
.buttons .play {
  width: 44px;
  background: #2f6fde;
  border-color: #2f6fde;
  color: white;
}
.time {
  font-variant-numeric: tabular-nums;
  min-width: 112px;
}
.clock {
  font-size: 19px;
  font-weight: 600;
  letter-spacing: 0.02em;
}
.date,
.dim {
  color: var(--dim);
  font-size: 11px;
}
.sel {
  min-width: 210px;
  font-variant-numeric: tabular-nums;
}
.toggles {
  display: flex;
  gap: 12px;
  color: var(--dim);
}
.toggles label {
  display: flex;
  align-items: center;
  gap: 4px;
  cursor: pointer;
}
.wheel {
  margin-left: auto;
  height: 26px;
  min-width: 100px;
}
.zoom {
  display: flex;
  gap: 4px;
}
.zoom button {
  min-width: 30px;
  height: 26px;
}
.master {
  display: flex;
  flex-direction: column;
  gap: 3px;
  width: 120px;
}
.meter {
  height: 4px;
  background: #2a2c33;
  border-radius: 2px;
  overflow: hidden;
}
.meter div {
  height: 100%;
  background: linear-gradient(90deg, #3fb871, #d4a72c 80%, #e5484d);
}
</style>
