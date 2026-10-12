<script setup lang="ts">
import { computed, onMounted, reactive, ref, watch } from 'vue';
import { download, makePlan, opusSupported, runExport, type ExportOptions, type Plan } from '../exporter';
import { ui } from '../state';
import { fmtClock, fmtDate, fmtDur, debounce } from '../util';

const span = ui.view.width * ui.view.msPerPx;
const opts = reactive<ExportOptions>({
  from: ui.selection?.from ?? ui.view.start,
  to: ui.selection?.to ?? ui.view.start + span,
  layout: 'downmix',
  format: 'wav',
  scope: 'audible',
  applyMix: true,
});
const plan = ref<Plan | null>(null);
const progress = ref<number | null>(null);
const error = ref('');
let abort: AbortController | null = null;

const refresh = debounce(async () => {
  error.value = '';
  try {
    plan.value = await makePlan({ ...opts });
  } catch (e) {
    error.value = (e as Error).message;
  }
}, 150);
watch(opts, refresh, { deep: true });
onMounted(refresh);

const fromText = computed(() => `${fmtDate(opts.from)} ${fmtClock(opts.from, true)}`);
const toText = computed(() => `${fmtDate(opts.to)} ${fmtClock(opts.to, true)}`);
const sizeEstimate = computed(() => {
  const p = plan.value;
  if (!p) return '';
  const secs = (opts.to - opts.from) / 1000;
  const ch = Math.max(1, p.outputs.length);
  const bytes = opts.format === 'wav' ? secs * 48000 * 2 * ch : secs * 3000 * ch;
  return bytes > 1e9 ? `${(bytes / 1e9).toFixed(1)} GB` : `${(bytes / 1e6).toFixed(1)} MB`;
});

async function start() {
  if (!plan.value || plan.value.errors.length) return;
  abort = new AbortController();
  progress.value = 0;
  error.value = '';
  try {
    const { blob, filename } = await runExport({ ...opts }, plan.value, (f) => (progress.value = f), abort.signal);
    download(blob, filename);
    ui.exportOpen = false;
  } catch (e) {
    if ((e as Error).name !== 'AbortError') error.value = (e as Error).message;
  } finally {
    progress.value = null;
  }
}

function close() {
  abort?.abort();
  ui.exportOpen = false;
}
</script>

<template>
  <div class="backdrop" @click.self="close">
    <div class="dialog">
      <h2>Export</h2>
      <div class="range">
        <div><span class="dim">From</span> {{ fromText }}</div>
        <div><span class="dim">To</span> {{ toText }}</div>
        <div><span class="dim">Length</span> {{ fmtDur(opts.to - opts.from) }}</div>
        <div class="dim small" v-if="!ui.selection">No selection — exporting the visible range. Drag on the timeline to choose a range.</div>
      </div>

      <fieldset>
        <legend>Layout</legend>
        <label><input type="radio" value="downmix" v-model="opts.layout" /> <b>Downmix</b> — everything mixed to one mono track</label>
        <label><input type="radio" value="tracks" v-model="opts.layout" /> <b>Multichannel</b> — one channel per speaker track (Opus: one file per track)</label>
        <label><input type="radio" value="channels" v-model="opts.layout" /> <b>Per Mumble channel</b> — one channel per voice channel (Opus: one file per channel)</label>
        <label><input type="radio" value="stems" v-model="opts.layout" /> <b>Stems</b> — zip with one aligned mono file per track</label>
      </fieldset>

      <div class="cols">
        <fieldset>
          <legend>Format</legend>
          <label><input type="radio" value="wav" v-model="opts.format" /> WAV (16-bit PCM)</label>
          <label :class="{ disabled: !opusSupported }"><input type="radio" value="opus" v-model="opts.format" :disabled="!opusSupported" /> Opus (compact{{ opusSupported ? '' : ', unsupported in this browser' }})</label>
        </fieldset>
        <fieldset>
          <legend>Tracks</legend>
          <label><input type="radio" value="audible" v-model="opts.scope" /> As heard (filters, mute, solo)</label>
          <label><input type="radio" value="visible" v-model="opts.scope" /> Visible tracks (ignore mute/solo)</label>
          <label><input type="radio" value="all" v-model="opts.scope" /> All tracks</label>
          <label class="sub"><input type="checkbox" v-model="opts.applyMix" /> Apply track volumes</label>
        </fieldset>
      </div>

      <div class="plan" v-if="plan">
        <div class="dim">{{ plan.segs.length }} clips · {{ fmtDur(plan.speechMs) }} of speech · ≈ {{ sizeEstimate }}</div>
        <ol v-if="plan.outputs.length && opts.layout !== 'downmix'">
          <li v-for="o in plan.outputs" :key="o.label">{{ o.label }}</li>
        </ol>
        <div v-for="w in plan.warnings" :key="w" class="warn">{{ w }}</div>
        <div v-for="e in plan.errors" :key="e" class="err">{{ e }}</div>
      </div>
      <div v-if="error" class="err">{{ error }}</div>

      <div class="progress" v-if="progress !== null"><div :style="{ width: `${progress * 100}%` }" /></div>

      <div class="actions">
        <span class="dim small" v-if="plan">{{ plan.filename }}</span>
        <button @click="close">{{ progress !== null ? 'Cancel' : 'Close' }}</button>
        <button class="primary" :disabled="!plan || !!plan.errors.length || progress !== null" @click="start">Download</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.55);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
}
.dialog {
  width: 620px;
  max-width: calc(100vw - 40px);
  max-height: calc(100vh - 40px);
  overflow: auto;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 10px;
  padding: 18px 20px;
  font-size: 13px;
  box-shadow: 0 20px 60px rgba(0, 0, 0, 0.5);
}
h2 {
  margin: 0 0 12px;
  font-size: 16px;
}
.range {
  display: grid;
  grid-template-columns: 1fr 1fr 1fr;
  gap: 4px 12px;
  font-variant-numeric: tabular-nums;
  margin-bottom: 12px;
}
.range .small {
  grid-column: 1 / -1;
}
fieldset {
  border: 1px solid var(--line);
  border-radius: 6px;
  margin: 0 0 10px;
  padding: 8px 12px;
  display: flex;
  flex-direction: column;
  gap: 5px;
}
legend {
  color: var(--dim);
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.06em;
}
label {
  display: flex;
  gap: 6px;
  align-items: center;
  cursor: pointer;
}
label.sub {
  margin-top: 4px;
}
label.disabled {
  opacity: 0.5;
}
.cols {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px;
}
.plan {
  margin: 6px 0;
}
.plan ol {
  margin: 6px 0;
  padding-left: 22px;
  max-height: 140px;
  overflow: auto;
}
.dim {
  color: var(--dim);
}
.small {
  font-size: 11px;
}
.warn {
  color: #d4a72c;
  margin-top: 4px;
}
.err {
  color: #ff8a84;
  margin-top: 4px;
}
.progress {
  height: 6px;
  background: #2a2c33;
  border-radius: 3px;
  overflow: hidden;
  margin: 10px 0;
}
.progress div {
  height: 100%;
  background: #2f6fde;
  transition: width 0.15s;
}
.actions {
  display: flex;
  gap: 8px;
  align-items: center;
  justify-content: flex-end;
  margin-top: 12px;
}
.actions .small {
  margin-right: auto;
}
</style>
