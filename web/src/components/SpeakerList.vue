<script setup lang="ts">
// Speaker filter for servers with very many users: virtualized, searchable,
// ranked by speaking time in the viewed day(s).
import { computed, ref } from 'vue';
import { scope } from '../rows';
import { activity, dataVersion, speakerList, ui } from '../state';
import { speakerColor } from '../util';
import VirtualList from './VirtualList.vue';
import type { Speaker } from '../api';

const query = ref('');
const activeOnly = ref(true);

const talk = computed(() => {
  void dataVersion.value;
  return activity(scope.value.from, scope.value.to).bySpeaker;
});

const items = computed(() => {
  const q = query.value.trim().toLowerCase();
  const t = talk.value;
  return speakerList.value
    .filter((s) => (!activeOnly.value || t.has(s.id)) && (!q || s.name.toLowerCase().includes(q)))
    .sort((a, b) => (t.get(b.id) ?? 0) - (t.get(a.id) ?? 0) || a.name.localeCompare(b.name));
});

function toggle(id: number) {
  if (ui.hiddenSpeakers.has(id)) ui.hiddenSpeakers.delete(id);
  else ui.hiddenSpeakers.add(id);
}

function only(id: number) {
  ui.hiddenSpeakers.clear();
  for (const s of speakerList.value) if (s.id !== id) ui.hiddenSpeakers.add(s.id);
}

/** Apply to everything currently listed (e.g. after a search). */
function setListed(show: boolean) {
  for (const s of items.value) {
    if (show) ui.hiddenSpeakers.delete(s.id);
    else ui.hiddenSpeakers.add(s.id);
  }
}

const shortDur = (ms: number) => (ms >= 60_000 ? `${Math.round(ms / 60_000)}m` : `${Math.max(1, Math.round(ms / 1000))}s`);
</script>

<template>
  <div class="slist">
    <header>
      <h3>Speakers <span class="dim">{{ items.length }}/{{ speakerList.length }}</span></h3>
      <label class="opt" title="Only speakers heard in the viewed day(s)"><input type="checkbox" v-model="activeOnly" /> active</label>
      <button class="link" v-if="ui.hiddenSpeakers.size" @click="ui.hiddenSpeakers.clear()">show all</button>
    </header>
    <div class="searchrow">
      <input class="search" v-model="query" placeholder="Search speakers…" />
      <button class="link" @click="setListed(true)" title="Show all listed">all</button>
      <button class="link" @click="setListed(false)" title="Hide all listed">none</button>
    </div>
    <VirtualList :items="items" :row-height="22" :key-of="(s: Speaker) => s.id">
      <template #default="{ item }">
        <div class="sp" :title="item.userId !== null ? `registered user #${item.userId}` : ''">
          <input type="checkbox" :checked="!ui.hiddenSpeakers.has(item.id)" @change="toggle(item.id)" />
          <span class="dot" :style="{ background: speakerColor(item.id) }" />
          <span class="label">{{ item.name }}</span>
          <span class="act" v-if="talk.get(item.id)">{{ shortDur(talk.get(item.id)!) }}</span>
          <button class="link only" @click="only(item.id)">only</button>
        </div>
      </template>
    </VirtualList>
  </div>
</template>

<style scoped>
.slist {
  display: flex;
  flex-direction: column;
  min-height: 0;
  flex: 2;
  padding-top: 8px;
  border-top: 1px solid var(--line);
}
header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 10px 4px;
}
h3 {
  margin: 0;
  flex: 1;
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.06em;
  color: var(--dim);
}
.opt {
  display: flex;
  gap: 3px;
  align-items: center;
  color: var(--dim);
  font-size: 11px;
}
.searchrow {
  display: flex;
  gap: 4px;
  align-items: center;
  padding: 0 10px 6px;
}
.search {
  flex: 1;
  min-width: 0;
}
.sp {
  height: 22px;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 0 10px;
  white-space: nowrap;
}
.sp:hover {
  background: rgba(255, 255, 255, 0.04);
}
.dot {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  flex: none;
}
.label {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
}
.act {
  color: #7fd19b;
  font-size: 10px;
  font-variant-numeric: tabular-nums;
}
.only {
  display: none;
}
.sp:hover .only {
  display: inline;
}
.sp:hover .act {
  display: none;
}
.dim {
  color: var(--dim);
  font-weight: 400;
}
</style>
