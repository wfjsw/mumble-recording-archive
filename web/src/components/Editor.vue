<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { Seg } from '../api';
import * as audio from '../audio';
import { engine } from '../engine';
import { COLLAPSED_H, RULER_H, contentHeight, rows, scope, type Row } from '../rows';
import {
  catalog,
  channelPath,
  channelStrip,
  dataVersion,
  ensureRange,
  presenceIn,
  segmentsIn,
  speakerName,
  trackGain,
  trackStrip,
  ui,
  uptimeIn,
} from '../state';
import {
  DAY,
  HOUR,
  channelColor,
  clamp,
  fmtClock,
  fmtDate,
  fmtDur,
  fmtTick,
  speakerColor,
  startOfDay,
  tickStep,
} from '../util';

const overview = ref<HTMLCanvasElement>();
const ruler = ref<HTMLCanvasElement>();
const lanes = ref<HTMLCanvasElement>();
const lanesBox = ref<HTMLDivElement>();
const tooltip = ref<{ x: number; y: number; lines: string[] } | null>(null);
const lanesHeight = ref(400);

const OVERVIEW_H = 44;
const MIN_MS_PER_PX = 0.05;
const MAX_MS_PER_PX = (14 * DAY) / 800;

const viewEnd = () => ui.view.start + ui.view.width * ui.view.msPerPx;
const xOf = (t: number) => (t - ui.view.start) / ui.view.msPerPx;
const tOf = (x: number) => ui.view.start + x * ui.view.msPerPx;

watch(
  scope,
  (s) => {
    void ensureRange(s.from, s.to);
  },
  { immediate: true },
);

// ---------- drawing ----------

let dirty = true;
let raf = 0;
const invalidate = () => (dirty = true);
watch(
  () => [
    ui.view.start,
    ui.view.msPerPx,
    ui.view.width,
    ui.scrollY,
    ui.selection,
    ui.playhead,
    dataVersion.value,
    rows.value,
    JSON.stringify(ui.mix),
    ui.hiddenChannels.size,
    ui.hiddenSpeakers.size,
  ],
  invalidate,
);

function setupCanvas(c: HTMLCanvasElement, w: number, h: number): CanvasRenderingContext2D {
  const dpr = window.devicePixelRatio || 1;
  if (c.width !== Math.round(w * dpr) || c.height !== Math.round(h * dpr)) {
    c.width = Math.round(w * dpr);
    c.height = Math.round(h * dpr);
    c.style.width = `${w}px`;
    c.style.height = `${h}px`;
  }
  const ctx = c.getContext('2d')!;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  return ctx;
}

function frame() {
  raf = requestAnimationFrame(frame);
  if (ui.playing) {
    ui.playhead = engine.now();
    if (ui.follow) {
      const x = xOf(ui.playhead);
      const W = ui.view.width;
      if (x > W * 0.92 || x < 0) ui.view.start = ui.playhead - W * ui.view.msPerPx * 0.08;
    }
    dirty = true;
  }
  if (!dirty) return;
  dirty = false;
  drawOverview();
  drawRuler();
  drawLanes();
}

function peakAt(s: Seg, t0: number, t1: number, pcm: Float32Array | undefined): number {
  if (pcm) {
    const a = clamp(Math.floor((t0 - s.startMs) * 48), 0, pcm.length);
    const b = clamp(Math.ceil((t1 - s.startMs) * 48), a + 1, pcm.length);
    let m = 0;
    for (let i = a; i < b; i++) {
      const v = pcm[i] < 0 ? -pcm[i] : pcm[i];
      if (v > m) m = v;
    }
    return Math.sqrt(Math.min(1, m));
  }
  const p = s.peaks;
  if (!p || !p.length) return 0.5;
  const pps = catalog.value?.peaksPerSec ?? 10;
  const a = clamp(Math.floor(((t0 - s.startMs) / 1000) * pps), 0, p.length - 1);
  const b = clamp(Math.ceil(((t1 - s.startMs) / 1000) * pps), a + 1, p.length);
  let m = 0;
  for (let i = a; i < b; i++) if (p[i] > m) m = p[i];
  return m / 255;
}

function drawClip(ctx: CanvasRenderingContext2D, s: Seg, top: number, h: number, label: string | null, W: number) {
  const x0 = xOf(s.startMs);
  const x1 = xOf(s.endMs);
  if (x1 < 0 || x0 > W) return;
  const audible = trackGain(s.channelId, s.speakerId) > 0;
  const col = (a: number) => (audible ? speakerColor(s.speakerId, a) : `rgba(140,140,150,${a})`);
  const w = Math.max(1, x1 - x0);
  ctx.fillStyle = col(0.18);
  ctx.fillRect(x0, top + 2, w, h - 4);
  const mid = top + h / 2;
  const amp = h / 2 - 4;
  if (w >= 3) {
    const pcm = ui.view.msPerPx < 40 ? audio.cached(s.id) : undefined;
    ctx.fillStyle = col(0.9);
    const from = Math.max(0, Math.floor(x0));
    const to = Math.min(W, Math.ceil(x1));
    for (let px = from; px < to; px++) {
      const d = peakAt(s, tOf(px), tOf(px + 1), pcm) * amp;
      ctx.fillRect(px, mid - d, 1, Math.max(1, 2 * d));
    }
  }
  ctx.fillStyle = col(1);
  ctx.fillRect(x0, top + 2, w, 1.5);
  if (s.target === 1 || s.target === 2) {
    ctx.fillStyle = 'rgba(255,200,80,.9)';
    ctx.fillRect(x0, top + h - 4, w, 2);
  }
  if (label && w > 46) {
    ctx.save();
    ctx.beginPath();
    ctx.rect(x0, top, w, h);
    ctx.clip();
    ctx.fillStyle = 'rgba(235,236,240,.92)';
    ctx.font = '11px system-ui, sans-serif';
    ctx.fillText(label, Math.max(x0, 0) + 4, top + 14);
    ctx.restore();
  }
}

function drawLanes() {
  const c = lanes.value;
  const box = lanesBox.value;
  if (!c || !box) return;
  const W = ui.view.width;
  const H = lanesHeight.value;
  const ctx = setupCanvas(c, W, H);
  ctx.fillStyle = '#17181c';
  ctx.fillRect(0, 0, W, H);
  const from = ui.view.start;
  const to = viewEnd();

  // grid
  const step = tickStep(ui.view.msPerPx);
  ctx.fillStyle = 'rgba(255,255,255,.045)';
  for (let t = Math.ceil(from / step) * step; t < to; t += step) ctx.fillRect(Math.round(xOf(t)), 0, 1, H);

  const segs = segmentsIn(from, to);
  const byTrack = new Map<string, Seg[]>();
  const byChannel = new Map<number, Seg[]>();
  for (const s of segs) {
    const k = `${s.channelId}:${s.speakerId}`;
    (byTrack.get(k) ?? byTrack.set(k, []).get(k)!).push(s);
    (byChannel.get(s.channelId) ?? byChannel.set(s.channelId, []).get(s.channelId)!).push(s);
  }
  const presence = new Map<string, [number, number][]>();
  for (const p of presenceIn(from, to)) {
    const k = `${p.channelId}:${p.speakerId}`;
    (presence.get(k) ?? presence.set(k, []).get(k)!).push([p.startMs, p.endMs ?? Date.now()]);
  }

  for (const r of rows.value) {
    const top = r.y - ui.scrollY;
    if (top > H || top + r.h < 0) continue;
    if (r.kind === 'channel') {
      ctx.fillStyle = '#212329';
      ctx.fillRect(0, top, W, r.h);
      ctx.fillStyle = channelColor(r.channelId, 0.8);
      ctx.fillRect(0, top + r.h - 1, W, 1);
      if (r.h === COLLAPSED_H) {
        for (const s of byChannel.get(r.channelId) ?? []) {
          if (r.speakers.includes(s.speakerId)) drawClip(ctx, s, top + 4, r.h - 8, speakerName(s.speakerId), W);
        }
      }
      continue;
    }
    ctx.fillStyle = (r.y / 58) % 2 ? '#1a1b20' : '#1c1d22';
    ctx.fillRect(0, top, W, r.h);
    // presence: speaker was in this channel
    ctx.fillStyle = 'rgba(120,160,255,.06)';
    for (const [a, b] of presence.get(r.key) ?? []) {
      const x0 = Math.max(0, xOf(a));
      const x1 = Math.min(W, xOf(b));
      if (x1 > x0) ctx.fillRect(x0, top, x1 - x0, r.h);
    }
    for (const s of byTrack.get(r.key) ?? []) drawClip(ctx, s, top + 3, r.h - 6, null, W);
    ctx.fillStyle = 'rgba(0,0,0,.35)';
    ctx.fillRect(0, top + r.h - 1, W, 1);
  }

  drawSelectionAndPlayhead(ctx, H);

  // request exact waveforms when zoomed in far enough
  if (ui.view.msPerPx < 40) {
    const vis = segs.filter((s) => !audio.cached(s.id)).slice(0, 40);
    if (vis.length) Promise.all(vis.map((s) => audio.load(s))).then(invalidate);
  }
}

function drawSelectionAndPlayhead(ctx: CanvasRenderingContext2D, H: number) {
  const sel = ui.selection;
  if (sel) {
    const x0 = xOf(sel.from);
    const x1 = xOf(sel.to);
    ctx.fillStyle = 'rgba(110,150,255,.16)';
    ctx.fillRect(x0, 0, x1 - x0, H);
    ctx.fillStyle = 'rgba(140,170,255,.7)';
    ctx.fillRect(x0, 0, 1, H);
    ctx.fillRect(x1, 0, 1, H);
  }
  const px = xOf(ui.playhead);
  ctx.fillStyle = '#ff5a52';
  ctx.fillRect(Math.round(px), 0, 1.5, H);
}

function drawRuler() {
  const c = ruler.value;
  if (!c) return;
  const W = ui.view.width;
  const ctx = setupCanvas(c, W, RULER_H);
  ctx.fillStyle = '#202227';
  ctx.fillRect(0, 0, W, RULER_H);
  const from = ui.view.start;
  const to = viewEnd();
  // recording coverage strip: green where the bot was connected
  ctx.fillStyle = 'rgba(255,255,255,.06)';
  ctx.fillRect(0, RULER_H - 4, W, 3);
  ctx.fillStyle = 'rgba(70,190,120,.7)';
  for (const [a, b] of uptimeIn(from, to)) {
    const x0 = Math.max(0, xOf(a));
    const x1 = Math.min(W, xOf(b));
    if (x1 > x0) ctx.fillRect(x0, RULER_H - 4, Math.max(1, x1 - x0), 3);
  }
  const step = tickStep(ui.view.msPerPx);
  const minor = step / 5;
  ctx.fillStyle = 'rgba(255,255,255,.25)';
  for (let t = Math.ceil(from / minor) * minor; t < to; t += minor) ctx.fillRect(Math.round(xOf(t)), RULER_H - 9, 1, 4);
  ctx.font = '11px system-ui, sans-serif';
  // align to local midnight for day-level steps
  const base = step >= HOUR ? startOfDay(from) : 0;
  for (let t = base + Math.ceil((from - base) / step) * step; t < to; t += step) {
    const x = Math.round(xOf(t));
    ctx.fillStyle = 'rgba(255,255,255,.55)';
    ctx.fillRect(x, 6, 1, RULER_H - 10);
    ctx.fillStyle = '#c9cbd3';
    ctx.fillText(fmtTick(t, step), x + 4, 15);
  }
  const sel = ui.selection;
  if (sel) {
    ctx.fillStyle = 'rgba(110,150,255,.35)';
    ctx.fillRect(xOf(sel.from), 0, xOf(sel.to) - xOf(sel.from), RULER_H - 5);
  }
  const px = xOf(ui.playhead);
  ctx.fillStyle = '#ff5a52';
  ctx.beginPath();
  ctx.moveTo(px - 6, 0);
  ctx.lineTo(px + 6, 0);
  ctx.lineTo(px, 9);
  ctx.fill();
  ctx.fillRect(Math.round(px), 0, 1.5, RULER_H);
}

const overviewRange = computed(() => {
  const span = ui.view.width * ui.view.msPerPx;
  if (span > 12 * HOUR) return { from: ui.view.start - span, to: ui.view.start + 2 * span };
  const mid = ui.view.start + span / 2;
  const d = startOfDay(mid);
  return { from: d, to: d + DAY };
});
watch(overviewRange, (r) => void ensureRange(r.from, r.to), { immediate: true });

function drawOverview() {
  const c = overview.value;
  if (!c) return;
  const W = c.parentElement!.clientWidth;
  const ctx = setupCanvas(c, W, OVERVIEW_H);
  ctx.fillStyle = '#121317';
  ctx.fillRect(0, 0, W, OVERVIEW_H);
  const { from, to } = overviewRange.value;
  const ms = (to - from) / W;
  const ox = (t: number) => (t - from) / ms;
  const n = Math.max(1, Math.floor(W / 2));
  const bucket = (to - from) / n;
  const stacks = new Map<number, Float32Array>();
  for (const s of segmentsIn(from, to)) {
    if (ui.hiddenChannels.has(s.channelId) || ui.hiddenSpeakers.has(s.speakerId)) continue;
    const arr = stacks.get(s.channelId) ?? stacks.set(s.channelId, new Float32Array(n)).get(s.channelId)!;
    const a = Math.max(s.startMs, from);
    const b = Math.min(s.endMs, to);
    for (let i = Math.floor((a - from) / bucket); i <= Math.min(n - 1, Math.floor((b - from) / bucket)); i++) {
      const bs = from + i * bucket;
      arr[i] += Math.max(0, Math.min(b, bs + bucket) - Math.max(a, bs));
    }
  }
  const h = OVERVIEW_H - 14;
  const base = new Float32Array(n);
  for (const [ch, arr] of stacks) {
    ctx.fillStyle = channelColor(ch, 0.85);
    for (let i = 0; i < n; i++) {
      if (!arr[i]) continue;
      const v = Math.min(1, Math.sqrt(arr[i] / bucket)) * h;
      ctx.fillRect(i * 2, OVERVIEW_H - 2 - base[i] - v, 2, v);
      base[i] = Math.min(h, base[i] + v);
    }
  }
  // hour ticks + labels
  ctx.font = '10px system-ui, sans-serif';
  const step = tickStep(ms, 70);
  const b0 = startOfDay(from);
  for (let t = b0 + Math.ceil((from - b0) / step) * step; t < to; t += step) {
    ctx.fillStyle = 'rgba(255,255,255,.12)';
    ctx.fillRect(Math.round(ox(t)), 0, 1, OVERVIEW_H);
    ctx.fillStyle = 'rgba(255,255,255,.45)';
    ctx.fillText(fmtTick(t, step), ox(t) + 3, 10);
  }
  // viewport
  const vx0 = ox(ui.view.start);
  const vx1 = ox(viewEnd());
  ctx.fillStyle = 'rgba(255,255,255,.08)';
  ctx.fillRect(vx0, 0, Math.max(2, vx1 - vx0), OVERVIEW_H);
  ctx.strokeStyle = 'rgba(255,255,255,.55)';
  ctx.strokeRect(vx0 + 0.5, 0.5, Math.max(2, vx1 - vx0) - 1, OVERVIEW_H - 1);
  ctx.fillStyle = '#ff5a52';
  ctx.fillRect(Math.round(ox(ui.playhead)), 0, 1, OVERVIEW_H);
}

// ---------- interaction ----------

function zoomAt(x: number, factor: number) {
  const t = tOf(x);
  const m = clamp(ui.view.msPerPx * factor, MIN_MS_PER_PX, MAX_MS_PER_PX);
  ui.view.msPerPx = m;
  ui.view.start = t - x * m;
}

/**
 * Wheel over the lanes: the plain wheel zooms or scrolls time depending on
 * ui.wheelMode, Shift+wheel does the other; Ctrl+wheel / pinch always zooms;
 * Alt+wheel scrolls tracks; horizontal wheel / swipe always pans.
 * Over the ruler the wheel always zooms; over the overview it always pans.
 */
function onWheel(e: WheelEvent, area: 'lanes' | 'ruler' | 'overview' = 'lanes') {
  e.preventDefault();
  const rect = lanes.value!.getBoundingClientRect();
  const x = clamp(e.clientX - rect.left, 0, ui.view.width);
  const dy = e.deltaMode === 1 ? e.deltaY * 30 : e.deltaY;
  const dx = e.deltaMode === 1 ? e.deltaX * 30 : e.deltaX;
  const pan = (d: number) => (ui.view.start += d * ui.view.msPerPx);
  const zoom = () => zoomAt(x, Math.exp(dy * (e.ctrlKey ? 0.01 : 0.002)));
  if (e.altKey) return scrollBy(dy || dx);
  if (Math.abs(dx) > Math.abs(dy)) return pan(dx);
  if (e.ctrlKey || e.metaKey) return zoom();
  let mode: 'zoom' | 'scroll' = area === 'ruler' ? 'zoom' : area === 'overview' ? 'scroll' : ui.wheelMode;
  if (e.shiftKey) mode = mode === 'zoom' ? 'scroll' : 'zoom';
  if (mode === 'zoom') zoom();
  else pan(dy * (area === 'overview' ? 4 : 1));
}

function scrollBy(dy: number) {
  ui.scrollY = clamp(ui.scrollY + dy, 0, Math.max(0, contentHeight.value - lanesHeight.value + 20));
}

function rowAt(y: number): Row | undefined {
  const yy = y + ui.scrollY;
  return rows.value.find((r) => yy >= r.y && yy < r.y + r.h);
}

function clipAt(x: number, y: number): Seg | undefined {
  const r = rowAt(y);
  if (!r) return;
  const t = tOf(x);
  const tol = 3 * ui.view.msPerPx;
  return segmentsIn(t - tol, t + tol).find(
    (s) => s.channelId === r.channelId && r.speakers.includes(s.speakerId) && s.startMs - tol <= t && s.endMs + tol >= t,
  );
}

type Drag = { mode: 'select' | 'pan' | 'overview'; x0: number; t0: number; start0: number; moved: boolean; shift: boolean };
let drag: Drag | null = null;

function onPointerDown(e: PointerEvent, area: 'lanes' | 'ruler') {
  const el = e.currentTarget as HTMLElement;
  el.setPointerCapture(e.pointerId);
  const x = e.clientX - el.getBoundingClientRect().left;
  const pan = e.button === 1 || e.altKey;
  drag = { mode: pan ? 'pan' : 'select', x0: x, t0: tOf(x), start0: ui.view.start, moved: false, shift: e.shiftKey };
  if (e.shiftKey && ui.selection && !pan) {
    // extend the existing selection from its far end
    const s = ui.selection;
    drag.t0 = Math.abs(tOf(x) - s.from) < Math.abs(tOf(x) - s.to) ? s.to : s.from;
  }
  void area;
}

function onPointerMove(e: PointerEvent) {
  const el = e.currentTarget as HTMLElement;
  const rect = el.getBoundingClientRect();
  const x = e.clientX - rect.left;
  const y = e.clientY - rect.top;
  if (!drag) {
    if (el === lanes.value) hover(x, y, e);
    return;
  }
  if (Math.abs(x - drag.x0) > 3) drag.moved = true;
  if (!drag.moved) return;
  if (drag.mode === 'pan') ui.view.start = drag.start0 - (x - drag.x0) * ui.view.msPerPx;
  else {
    const t = tOf(clamp(x, 0, ui.view.width));
    ui.selection = { from: Math.min(drag.t0, t), to: Math.max(drag.t0, t) };
  }
}

function onPointerUp(e: PointerEvent) {
  if (!drag) return;
  const el = e.currentTarget as HTMLElement;
  const x = e.clientX - el.getBoundingClientRect().left;
  if (!drag.moved && drag.mode === 'select') {
    if (!drag.shift) ui.selection = null;
    engine.seek(tOf(x));
  }
  drag = null;
}

function onDblClick(e: MouseEvent) {
  const rect = lanes.value!.getBoundingClientRect();
  const s = clipAt(e.clientX - rect.left, e.clientY - rect.top);
  if (s) {
    ui.selection = { from: s.startMs, to: s.endMs };
    engine.seek(s.startMs);
  }
}

function hover(x: number, y: number, e: PointerEvent) {
  const s = clipAt(x, y);
  if (!s) {
    tooltip.value = null;
    return;
  }
  const kind = s.target === 1 ? ' · shout' : s.target === 2 ? ' · whisper' : '';
  tooltip.value = {
    x: e.clientX + 14,
    y: e.clientY + 12,
    lines: [
      `${speakerName(s.speakerId)}${kind}`,
      channelPath(s.channelId),
      `${fmtDate(s.startMs)} ${fmtClock(s.startMs, true)}`,
      `${fmtDur(s.endMs - s.startMs)}${s.status === 'error' ? ' · original stream (re-encode failed)' : ''}`,
    ],
  };
}

function onOverviewWheel(e: WheelEvent) {
  onWheel(e, 'overview');
}

function overviewPointer(e: PointerEvent) {
  if (e.type === 'pointerdown') (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  if (e.type === 'pointermove' && !(e.buttons & 1)) return;
  const el = e.currentTarget as HTMLElement;
  const W = el.clientWidth;
  const { from, to } = overviewRange.value;
  const t = from + ((e.clientX - el.getBoundingClientRect().left) / W) * (to - from);
  const span = ui.view.width * ui.view.msPerPx;
  ui.view.start = t - span / 2;
}

// ---------- layout ----------

let ro: ResizeObserver | null = null;
onMounted(() => {
  ro = new ResizeObserver(() => {
    const box = lanesBox.value!;
    ui.view.width = Math.max(100, box.clientWidth);
    lanesHeight.value = Math.max(50, box.clientHeight - RULER_H);
    invalidate();
  });
  ro.observe(lanesBox.value!);
  ro.observe(overview.value!.parentElement!);
  raf = requestAnimationFrame(frame);
});
onBeforeUnmount(() => {
  ro?.disconnect();
  cancelAnimationFrame(raf);
});

const visibleRows = computed(() => rows.value.filter((r) => r.y + r.h > ui.scrollY && r.y < ui.scrollY + lanesHeight.value));
function trackCount(r: Row) {
  return r.speakers.length;
}
function toggleCollapse(id: number) {
  if (ui.collapsed.has(id)) ui.collapsed.delete(id);
  else ui.collapsed.add(id);
}
/** Compact channel label for deep trees: "… / Parent / Child". */
function crumb(id: number): { prefix: string; name: string } {
  const parts = channelPath(id).split(' / ');
  const name = parts.pop() ?? '';
  if (!parts.length) return { prefix: '', name };
  const parent = parts[parts.length - 1];
  return { prefix: `${parts.length > 1 ? '… / ' : ''}${parent} / `, name };
}

function collapseAll(collapse: boolean) {
  if (!collapse) ui.collapsed.clear();
  else for (const r of rows.value) if (r.kind === 'channel') ui.collapsed.add(r.channelId);
  ui.scrollY = 0;
}

function onGain() {
  engine.applyGains();
}
watch(() => JSON.stringify(ui.mix) + ui.hiddenChannels.size + ui.hiddenSpeakers.size, () => engine.applyGains());
</script>

<template>
  <div class="editor">
    <div class="overview">
      <canvas
        ref="overview"
        @pointerdown="overviewPointer"
        @pointermove="overviewPointer"
        @wheel.prevent="onOverviewWheel"
        title="Overview — click or drag to navigate, wheel to scroll"
      />
    </div>
    <div class="body">
      <div class="headers" @wheel.prevent="(e) => scrollBy(e.deltaY)">
        <div class="ruler-spacer" title="Wheel over lanes: zoom or scroll (toggle W), Shift+wheel: the other · wheel over ruler: zoom · wheel over overview: scroll · Alt+wheel: scroll tracks · drag: select · Alt/middle-drag: pan">
          <span>{{ fmtDate(ui.view.start) }}</span>
          <span class="spacer" />
          <button class="link" @click="collapseAll(true)" title="Collapse every channel to one merged lane">collapse</button>
          <button class="link" @click="collapseAll(false)" title="Expand all channels">expand</button>
        </div>
        <div class="header-rows">
          <div
            v-for="r in visibleRows"
            :key="r.key"
            class="row"
            :class="r.kind"
            :style="{ top: `${r.y - ui.scrollY}px`, height: `${r.h}px` }"
          >
            <template v-if="r.kind === 'channel'">
              <button class="caret" @click="toggleCollapse(r.channelId)" :title="ui.collapsed.has(r.channelId) ? 'Expand' : 'Collapse (merged channel lane)'">
                {{ ui.collapsed.has(r.channelId) ? '▸' : '▾' }}
              </button>
              <span class="swatch" :style="{ background: channelColor(r.channelId) }" />
              <span class="name" :title="channelPath(r.channelId)">
                <span class="crumb-dim" v-if="crumb(r.channelId).prefix">{{ crumb(r.channelId).prefix }}</span>{{ crumb(r.channelId).name }}
              </span>
              <span class="count">{{ trackCount(r) }}</span>
              <button
                class="ms"
                :class="{ on: channelStrip(r.channelId).mute }"
                @click="channelStrip(r.channelId).mute = !channelStrip(r.channelId).mute"
                title="Mute channel"
              >M</button>
              <button
                class="ms solo"
                :class="{ on: channelStrip(r.channelId).solo }"
                @click="channelStrip(r.channelId).solo = !channelStrip(r.channelId).solo"
                title="Solo channel — listen to this channel's reconstructed stream"
              >S</button>
            </template>
            <template v-else>
              <span class="bar" :style="{ background: speakerColor(r.speakerId!) }" />
              <div class="track-main">
                <div class="track-top">
                  <span class="name">{{ speakerName(r.speakerId!) }}</span>
                  <button class="ms" :class="{ on: trackStrip(r.key).mute }" @click="trackStrip(r.key).mute = !trackStrip(r.key).mute" title="Mute">M</button>
                  <button class="ms solo" :class="{ on: trackStrip(r.key).solo }" @click="trackStrip(r.key).solo = !trackStrip(r.key).solo" title="Solo">S</button>
                </div>
                <input
                  class="vol"
                  type="range"
                  min="0"
                  max="2"
                  step="0.01"
                  v-model.number="trackStrip(r.key).gain"
                  @input="onGain"
                  @dblclick="trackStrip(r.key).gain = 1"
                  :title="`Volume ${Math.round(trackStrip(r.key).gain * 100)}% (double-click to reset)`"
                />
              </div>
            </template>
          </div>
          <div v-if="!rows.length" class="empty">
            {{ catalog?.days.length ? 'No speech in this range.' : 'No recordings yet.' }}
          </div>
        </div>
      </div>
      <div class="lanes" ref="lanesBox">
        <canvas
          ref="ruler"
          class="ruler"
          @pointerdown="(e) => onPointerDown(e, 'ruler')"
          @pointermove="onPointerMove"
          @pointerup="onPointerUp"
          @wheel="(e: WheelEvent) => onWheel(e, 'ruler')"
          title="Wheel: zoom · drag: select · click: move playhead"
        />
        <canvas
          ref="lanes"
          @pointerdown="(e) => onPointerDown(e, 'lanes')"
          @pointermove="onPointerMove"
          @pointerup="onPointerUp"
          @pointerleave="tooltip = null"
          @dblclick="onDblClick"
          @wheel="(e: WheelEvent) => onWheel(e, 'lanes')"
          @auxclick.prevent
        />
      </div>
    </div>
    <div v-if="tooltip" class="tooltip" :style="{ left: `${tooltip.x}px`, top: `${tooltip.y}px` }">
      <div v-for="(l, i) in tooltip.lines" :key="i" :class="{ strong: i === 0 }">{{ l }}</div>
    </div>
  </div>
</template>

<style scoped>
.editor {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  flex: 1;
}
.overview {
  height: 44px;
  border-bottom: 1px solid var(--line);
  cursor: pointer;
}
.overview canvas {
  display: block;
}
.body {
  flex: 1;
  display: flex;
  min-height: 0;
}
.headers {
  width: 232px;
  flex: none;
  display: flex;
  flex-direction: column;
  border-right: 1px solid var(--line);
  background: var(--panel);
}
.ruler-spacer .spacer {
  flex: 1;
}
.crumb-dim {
  color: var(--dim);
  font-weight: 400;
}
.ruler-spacer {
  gap: 6px;
  height: 30px;
  flex: none;
  border-bottom: 1px solid var(--line);
  display: flex;
  align-items: center;
  padding: 0 10px;
  color: var(--dim);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}
.header-rows {
  position: relative;
  flex: 1;
  overflow: hidden;
}
.row {
  position: absolute;
  left: 0;
  right: 0;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 0 8px 0 4px;
  border-bottom: 1px solid rgba(0, 0, 0, 0.35);
  box-sizing: border-box;
}
.row.channel {
  background: #24262c;
  font-weight: 600;
  font-size: 12px;
}
.row.track {
  padding-left: 12px;
  background: #1e2025;
}
.caret {
  width: 18px;
  padding: 0;
  background: none;
  border: none;
  color: var(--dim);
}
.swatch {
  width: 8px;
  height: 8px;
  border-radius: 2px;
  flex: none;
}
.bar {
  width: 4px;
  align-self: stretch;
  margin: 6px 0;
  border-radius: 2px;
  flex: none;
}
.name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.count {
  color: var(--dim);
  font-weight: 400;
  font-size: 11px;
}
.track-main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.track-top {
  display: flex;
  align-items: center;
  gap: 4px;
  font-size: 13px;
}
.ms {
  width: 20px;
  height: 18px;
  padding: 0;
  font-size: 10px;
  font-weight: 700;
  border-radius: 3px;
  flex: none;
}
.ms.on {
  background: #c94b4b;
  border-color: #c94b4b;
  color: white;
}
.ms.solo.on {
  background: #d4a72c;
  border-color: #d4a72c;
  color: #111;
}
.vol {
  width: 100%;
  height: 12px;
  margin: 0;
}
.lanes {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.lanes canvas {
  display: block;
  touch-action: none;
}
.ruler {
  cursor: text;
  flex: none;
}
.empty {
  padding: 20px 12px;
  color: var(--dim);
  font-size: 13px;
}
.tooltip {
  position: fixed;
  z-index: 50;
  pointer-events: none;
  background: rgba(20, 21, 25, 0.96);
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 6px 9px;
  font-size: 12px;
  color: var(--dim);
  line-height: 1.5;
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.4);
}
.tooltip .strong {
  color: var(--text);
  font-weight: 600;
}
</style>
