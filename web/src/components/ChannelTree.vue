<script setup lang="ts">
// Channel filter tree built for very large, deeply nested servers:
//  - virtualized rows (thousands of channels)
//  - "active only" shows just branches with speech in the viewed days
//  - search shows matches with their ancestors
//  - focus (⤢) re-roots the view in a subtree, so depth never runs out of width
//  - checkboxes act on whole subtrees, with a mixed state for partial filters
import { computed, reactive, ref } from 'vue';
import { scope } from '../rows';
import { activity, dataVersion, subtree, tree, ui, type TreeNode } from '../state';
import { channelColor } from '../util';
import VirtualList from './VirtualList.vue';

const query = ref('');
const activeOnly = ref(true);
const focus = ref<number | null>(null);
/** Explicit expand/collapse choices; otherwise the default rule applies. */
const override = reactive(new Map<number, boolean>());

const INDENT = 10;
const MAX_INDENT_LEVELS = 14;

/** Speech ms per channel subtree within the viewed days. */
const subtreeActivity = computed(() => {
  void dataVersion.value;
  const { byChannel } = activity(scope.value.from, scope.value.to);
  const { order, nodes } = tree.value;
  const total = new Float64Array(order.length);
  // children come after parents in DFS order: accumulate bottom-up
  for (let i = order.length - 1; i >= 0; i--) {
    const n = nodes.get(order[i])!;
    total[i] += byChannel.get(order[i]) ?? 0;
    for (const c of n.children) total[i] += total[nodes.get(c)!.index];
  }
  return { own: byChannel, total };
});

/** Hidden counts per subtree via prefix sums over DFS order. */
const hiddenPrefix = computed(() => {
  const { order } = tree.value;
  const pre = new Uint32Array(order.length + 1);
  for (let i = 0; i < order.length; i++) pre[i + 1] = pre[i] + (ui.hiddenChannels.has(order[i]) ? 1 : 0);
  return pre;
});

function state(n: TreeNode): 'on' | 'off' | 'mixed' {
  const hidden = hiddenPrefix.value[n.end] - hiddenPrefix.value[n.index];
  return hidden === 0 ? 'on' : hidden === n.end - n.index ? 'off' : 'mixed';
}

const matches = computed(() => {
  const q = query.value.trim().toLowerCase();
  if (!q) return null;
  const { order, nodes } = tree.value;
  const keep = new Uint8Array(order.length);
  for (let i = 0; i < order.length; i++) {
    const n = nodes.get(order[i])!;
    if (n.channel.name.toLowerCase().includes(q)) {
      keep[i] = 2; // a match: show it (and its subtree on expand)
      // mark ancestors so the path stays visible
      let p = n.channel.parentId;
      let guard = 0;
      while (p !== null && guard++ < 10_000) {
        const pn = nodes.get(p);
        if (!pn || keep[pn.index]) break;
        keep[pn.index] = 1;
        p = pn.channel.parentId;
      }
    }
  }
  return keep;
});

const focusNode = computed(() => (focus.value !== null ? tree.value.nodes.get(focus.value) ?? null : null));

const breadcrumb = computed(() => {
  const out: TreeNode[] = [];
  let n = focusNode.value;
  let guard = 0;
  while (n && guard++ < 10_000) {
    out.unshift(n);
    n = n.channel.parentId !== null ? tree.value.nodes.get(n.channel.parentId) ?? null : null;
  }
  return out;
});

function expanded(n: TreeNode): boolean {
  const o = override.get(n.channel.id);
  if (o !== undefined) return o;
  if (matches.value) return matches.value[n.index] === 1; // expand ancestors of matches
  if (activeOnly.value) return true; // active branches are usually small; show them whole
  return n.depth - (focusNode.value?.depth ?? 0) < 1;
}

interface Item {
  node: TreeNode;
  rel: number;
}

const items = computed<Item[]>(() => {
  const { order, nodes } = tree.value;
  const act = subtreeActivity.value.total;
  const m = matches.value;
  const f = focusNode.value;
  const [from, to] = f ? [f.index, f.end] : [0, order.length];
  const base = f ? f.depth : 0;
  const out: Item[] = [];
  let i = from;
  while (i < to) {
    const n = nodes.get(order[i])!;
    const visible = (!activeOnly.value || act[i] > 0 || (m && m[i])) && (!m || m[i]);
    if (!visible) {
      i = n.end; // skip the whole subtree
      continue;
    }
    out.push({ node: n, rel: n.depth - base });
    i = expanded(n) || n.children.length === 0 ? i + 1 : n.end;
  }
  return out;
});

function toggleExpand(n: TreeNode) {
  override.set(n.channel.id, !expanded(n));
}

function toggleCheck(n: TreeNode) {
  const ids = subtree(n.channel.id);
  if (state(n) === 'on') for (const id of ids) ui.hiddenChannels.add(id);
  else for (const id of ids) ui.hiddenChannels.delete(id);
}

function only(n: TreeNode) {
  const keep = new Set(subtree(n.channel.id));
  ui.hiddenChannels.clear();
  for (const id of tree.value.order) if (!keep.has(id)) ui.hiddenChannels.add(id);
}

function setFocus(id: number | null) {
  focus.value = id;
}

const shortDur = (ms: number) => (ms >= 60_000 ? `${Math.round(ms / 60_000)}m` : `${Math.max(1, Math.round(ms / 1000))}s`);
const totalChannels = computed(() => tree.value.order.length);
</script>

<template>
  <div class="ctree">
    <header>
      <h3>Channels <span class="dim">{{ totalChannels }}</span></h3>
      <label class="opt" title="Only branches with speech in the viewed day(s)">
        <input type="checkbox" v-model="activeOnly" /> active
      </label>
      <button class="link" v-if="ui.hiddenChannels.size" @click="ui.hiddenChannels.clear()">show all</button>
    </header>
    <input class="search" v-model="query" placeholder="Search channels…" />
    <div class="crumbs" v-if="focusNode">
      <button class="link" @click="setFocus(null)">⌂</button>
      <template v-for="b in breadcrumb" :key="b.channel.id">
        <span class="sep">›</span>
        <button class="link crumb" @click="setFocus(b.channel.id)" :title="b.path">{{ b.channel.name }}</button>
      </template>
    </div>
    <VirtualList :items="items" :row-height="22" :key-of="(it: Item) => it.node.channel.id">
      <template #default="{ item }">
        <div
          class="node"
          :class="{ removed: item.node.channel.removedAt }"
          :style="{ paddingLeft: `${4 + Math.min(item.rel, MAX_INDENT_LEVELS) * INDENT}px` }"
          :title="item.node.path + (item.rel > MAX_INDENT_LEVELS ? ` (level ${item.node.depth})` : '')"
        >
          <button class="caret" :class="{ leaf: !item.node.children.length }" @click="toggleExpand(item.node)">
            {{ item.node.children.length ? (expanded(item.node) ? '▾' : '▸') : '' }}
          </button>
          <input
            type="checkbox"
            :checked="state(item.node) !== 'off'"
            :indeterminate="state(item.node) === 'mixed'"
            @change="toggleCheck(item.node)"
            title="Show/hide this channel and everything below it"
          />
          <span class="swatch" :style="{ background: channelColor(item.node.channel.id) }" />
          <span v-if="item.rel > MAX_INDENT_LEVELS" class="deep">{{ item.node.depth }}</span>
          <span class="label" @dblclick="setFocus(item.node.channel.id)">{{ item.node.channel.name }}</span>
          <span class="act" v-if="subtreeActivity.own.get(item.node.channel.id)">{{ shortDur(subtreeActivity.own.get(item.node.channel.id)!) }}</span>
          <span class="tools">
            <button class="link" @click="setFocus(item.node.channel.id)" title="Focus this subtree" v-if="item.node.children.length">⤢</button>
            <button class="link" @click="only(item.node)" title="Show only this subtree">only</button>
          </span>
        </div>
      </template>
    </VirtualList>
    <div class="empty" v-if="!items.length">
      {{ query ? 'No matching channels.' : activeOnly ? 'No speech in the viewed day(s).' : 'No channels.' }}
    </div>
  </div>
</template>

<style scoped>
.ctree {
  display: flex;
  flex-direction: column;
  min-height: 0;
  flex: 3;
  padding-top: 8px;
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
.search {
  margin: 0 10px 6px;
}
.crumbs {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 2px;
  padding: 0 10px 6px;
  font-size: 11px;
}
.crumb {
  max-width: 90px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.sep {
  color: var(--dim);
}
.node {
  height: 22px;
  display: flex;
  align-items: center;
  gap: 4px;
  padding-right: 8px;
  white-space: nowrap;
}
.node:hover {
  background: rgba(255, 255, 255, 0.04);
}
.node.removed .label {
  text-decoration: line-through;
  opacity: 0.6;
}
.caret {
  width: 14px;
  padding: 0;
  border: none;
  background: none;
  color: var(--dim);
  font-size: 10px;
  flex: none;
}
.swatch {
  width: 8px;
  height: 8px;
  border-radius: 2px;
  flex: none;
}
.deep {
  font-size: 9px;
  color: var(--dim);
  border: 1px solid var(--line);
  border-radius: 3px;
  padding: 0 3px;
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
  flex: none;
}
.tools {
  display: none;
  gap: 2px;
  flex: none;
}
.node:hover .tools {
  display: flex;
}
.node:hover .act {
  display: none;
}
.empty {
  padding: 8px 12px;
  color: var(--dim);
  font-size: 12px;
}
.dim {
  color: var(--dim);
  font-weight: 400;
}
</style>
