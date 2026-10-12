<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue';
import { loadConfig } from './api';
import Editor from './components/Editor.vue';
import ExportDialog from './components/ExportDialog.vue';
import Sidebar from './components/Sidebar.vue';
import TopBar from './components/TopBar.vue';
import Transport from './components/Transport.vue';
import { engine } from './engine';
import { catalog, ensureRange, refreshCatalog, segmentsIn, startAutoRefresh, ui } from './state';
import { MIN } from './util';

const transport = ref<InstanceType<typeof Transport>>();

onMounted(async () => {
  await loadConfig();
  await refreshCatalog();
  startAutoRefresh();
  window.addEventListener('keydown', onKey);
  // Start on the most recent activity (up to 15 minutes of it), once the
  // timeline has its real width.
  requestAnimationFrame(async () => {
    const latest = catalog.value?.stats.latestMs;
    if (!latest) return;
    await ensureRange(latest - 15 * MIN, latest);
    const recent = segmentsIn(latest - 15 * MIN, latest + 1);
    const from = recent.length ? recent[0].startMs : latest - 15 * MIN;
    const pad = Math.max(2000, (latest - from) * 0.04);
    ui.view.msPerPx = (latest - from + 2 * pad) / ui.view.width;
    ui.view.start = from - pad;
    ui.playhead = from;
  });
});
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));

function onKey(e: KeyboardEvent) {
  const tag = (e.target as HTMLElement)?.tagName;
  if (tag === 'INPUT' && (e.target as HTMLInputElement).type !== 'range' && (e.target as HTMLInputElement).type !== 'checkbox') return;
  if (ui.exportOpen) {
    if (e.key === 'Escape') ui.exportOpen = false;
    return;
  }
  const span = ui.view.width * ui.view.msPerPx;
  switch (e.key) {
    case ' ':
      e.preventDefault();
      engine.toggle();
      break;
    case 'ArrowLeft':
      ui.view.start -= span * 0.2;
      break;
    case 'ArrowRight':
      ui.view.start += span * 0.2;
      break;
    case '+':
    case '=':
      transport.value?.zoom(1 / 1.6);
      break;
    case '-':
      transport.value?.zoom(1.6);
      break;
    case '[':
      transport.value?.jump(-1);
      break;
    case ']':
      transport.value?.jump(1);
      break;
    case 'z':
      transport.value?.fitSelection();
      break;
    case 'l':
      ui.loop = !ui.loop;
      break;
    case 'e':
      ui.exportOpen = true;
      break;
    case 'w':
      ui.wheelMode = ui.wheelMode === 'zoom' ? 'scroll' : 'zoom';
      break;
    case 'Escape':
      ui.selection = null;
      break;
    case 'Home':
      if (ui.selection) engine.seek(ui.selection.from);
      break;
    default:
      return;
  }
}
</script>

<template>
  <div class="app">
    <TopBar />
    <div class="main">
      <Sidebar />
      <Editor />
    </div>
    <Transport ref="transport" />
    <ExportDialog v-if="ui.exportOpen" />
  </div>
</template>

<style scoped>
.app {
  height: 100vh;
  display: flex;
  flex-direction: column;
}
.main {
  flex: 1;
  display: flex;
  min-height: 0;
}
</style>
