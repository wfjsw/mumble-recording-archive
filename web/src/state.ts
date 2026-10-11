import { computed, reactive, ref, shallowRef } from 'vue';
import {
  fetchCatalog,
  fetchChannels,
  fetchDay,
  fetchSpeakers,
  type Catalog,
  type Channel,
  type DayEntry,
  type DayIndex,
  type Seg,
  type Speaker,
} from './api';
import { MIN } from './util';

export interface Strip {
  mute: boolean;
  solo: boolean;
  gain: number;
}

export interface Selection {
  from: number;
  to: number;
}

export const ui = reactive({
  error: '',
  loading: false,
  /** Visible time window: left edge (unix ms) and zoom. */
  view: { start: Date.now() - 10 * MIN, msPerPx: 500, width: 1000 },
  scrollY: 0,
  playhead: Date.now() - 10 * MIN,
  playing: false,
  selection: null as Selection | null,
  follow: true,
  skipSilence: true,
  loop: false,
  hiddenChannels: new Set<number>(),
  hiddenSpeakers: new Set<number>(),
  collapsed: new Set<number>(),
  mix: {
    master: 1,
    tracks: {} as Record<string, Strip>,
    channels: {} as Record<number, Strip>,
  },
  exportOpen: false,
  sidebarWidth: 300,
  /** What a plain mouse wheel does over the track lanes (Shift+wheel does the other). */
  wheelMode: 'zoom' as 'zoom' | 'scroll',
});

export const catalog = shallowRef<Catalog | null>(null);
export const channelList = shallowRef<Channel[]>([]);
export const speakerList = shallowRef<Speaker[]>([]);
/** Bumped whenever loaded data changes, to trigger canvas redraws. */
export const dataVersion = ref(0);

export const speakers = computed(() => new Map(speakerList.value.map((s) => [s.id, s])));
export const channels = computed(() => new Map(channelList.value.map((c) => [c.id, c])));

export interface TreeNode {
  channel: Channel;
  depth: number;
  /** Position in depth-first order; the subtree is order[index, end). */
  index: number;
  end: number;
  children: number[];
  /** Path without the root channel, e.g. "Games / EU / Squad 3". */
  path: string;
}

/**
 * Channel tree in depth-first order (siblings by position, then name).
 * Built iteratively in O(n log n): safe for thousands of deeply nested channels.
 */
export const tree = computed(() => {
  const all = channelList.value;
  const ids = new Set(all.map((c) => c.id));
  const kids = new Map<number | null, Channel[]>();
  for (const c of all) {
    const k = c.parentId !== null && ids.has(c.parentId) && c.parentId !== c.id ? c.parentId : null;
    (kids.get(k) ?? kids.set(k, []).get(k)!).push(c);
  }
  for (const l of kids.values()) l.sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
  const order: number[] = [];
  const nodes = new Map<number, TreeNode>();
  // explicit stack instead of recursion (depth can be large)
  const stack: { c: Channel; depth: number; path: string; exit: boolean }[] = [];
  const roots = kids.get(null) ?? [];
  for (let i = roots.length - 1; i >= 0; i--) stack.push({ c: roots[i], depth: 0, path: '', exit: false });
  while (stack.length) {
    const it = stack.pop()!;
    if (it.exit) {
      nodes.get(it.c.id)!.end = order.length;
      continue;
    }
    if (nodes.has(it.c.id)) continue; // cycle guard
    const children = kids.get(it.c.id) ?? [];
    const path = it.depth === 0 ? it.c.name : it.depth === 1 ? it.c.name : `${it.path} / ${it.c.name}`;
    nodes.set(it.c.id, {
      channel: it.c,
      depth: it.depth,
      index: order.length,
      end: order.length + 1,
      children: children.map((c) => c.id),
      path,
    });
    order.push(it.c.id);
    stack.push({ ...it, exit: true });
    const childPath = it.depth === 0 ? '' : path;
    for (let i = children.length - 1; i >= 0; i--) stack.push({ c: children[i], depth: it.depth + 1, path: childPath, exit: false });
  }
  return { order, nodes };
});

/** Channels in tree order with depth (kept for simple consumers). */
export const channelTree = computed(() =>
  tree.value.order.map((id) => ({ channel: tree.value.nodes.get(id)!.channel, depth: tree.value.nodes.get(id)!.depth })),
);

export function channelPath(id: number): string {
  return tree.value.nodes.get(id)?.path ?? channels.value.get(id)?.name ?? `#${id}`;
}

/** Ids of a channel and all its descendants. */
export function subtree(id: number): number[] {
  const n = tree.value.nodes.get(id);
  return n ? tree.value.order.slice(n.index, n.end) : [id];
}

export const speakerName = (id: number) => speakers.value.get(id)?.name ?? `#${id}`;

// ---------- day indexes ----------

const days = new Map<string, DayIndex>();
const loadingDays = new Map<string, Promise<void>>();
/** Longest stored segment; used to widen overlap lookups. */
const MAX_SEG_MS = 6 * MIN;

export async function refreshCatalog() {
  try {
    ui.loading = true;
    const [cat, ch, sp] = await Promise.all([fetchCatalog(), fetchChannels(), fetchSpeakers()]);
    catalog.value = cat;
    channelList.value = ch;
    speakerList.value = sp;
    ui.error = '';
    // Re-read day files already loaded (cheap 304 when unchanged).
    await Promise.all([...days.keys()].map((d) => loadDay(d, true)));
  } catch (e) {
    ui.error = `Cannot load archive catalog: ${(e as Error).message}`;
  } finally {
    ui.loading = false;
  }
}

function dayEntries(from: number, to: number): DayEntry[] {
  return (catalog.value?.days ?? []).filter((d) => d.to > from - MAX_SEG_MS && d.from < to);
}

function loadDay(day: string, force = false): Promise<void> {
  if (!force && days.has(day)) return Promise.resolve();
  const existing = loadingDays.get(day);
  if (existing) return existing;
  const entry = catalog.value?.days.find((d) => d.day === day);
  if (!entry) return Promise.resolve();
  const p = fetchDay(entry.file)
    .then((idx) => {
      days.set(day, idx);
      dataVersion.value++;
    })
    .catch((e) => console.warn('day index', day, e))
    .finally(() => loadingDays.delete(day));
  loadingDays.set(day, p);
  return p;
}

/** Make sure all day indexes overlapping [from, to) are loaded. */
export function ensureRange(from: number, to: number): Promise<void> {
  return Promise.all(dayEntries(from, to).map((d) => loadDay(d.day))).then(() => {});
}

function loadedDays(from: number, to: number): DayIndex[] {
  const out: DayIndex[] = [];
  for (const d of days.values()) if (d.to > from - MAX_SEG_MS && d.from < to) out.push(d);
  return out.sort((a, b) => a.from - b.from);
}

/** Loaded segments overlapping [from, to), ordered by start. */
export function segmentsIn(from: number, to: number): Seg[] {
  const out: Seg[] = [];
  for (const d of loadedDays(from, to)) {
    const s = d.segments;
    // segments are sorted by start: binary search the first candidate
    let lo = 0;
    let hi = s.length;
    while (lo < hi) {
      const m = (lo + hi) >> 1;
      if (s[m].startMs < from - MAX_SEG_MS) lo = m + 1;
      else hi = m;
    }
    for (let i = lo; i < s.length && s[i].startMs < to; i++) if (s[i].endMs > from) out.push(s[i]);
  }
  return out;
}

export function presenceIn(from: number, to: number) {
  const seen = new Set<string>();
  const out = [];
  for (const d of loadedDays(from, to)) {
    for (const p of d.presence) {
      const k = `${p.speakerId}:${p.channelId}:${p.startMs}`;
      if (seen.has(k) || p.startMs >= to || (p.endMs !== null && p.endMs <= from)) continue;
      seen.add(k);
      out.push(p);
    }
  }
  return out;
}

export function uptimeIn(from: number, to: number): [number, number][] {
  const out: [number, number][] = [];
  for (const d of loadedDays(from, to)) for (const u of d.uptime) if (u[0] < to && u[1] > from) out.push(u);
  return out.sort((a, b) => a[0] - b[0]);
}

/** First audible segment starting after t, loading further days as needed. */
export async function nextSegment(t: number, accept: (s: Seg) => boolean): Promise<Seg | null> {
  const later = (catalog.value?.days ?? []).filter((d) => d.to > t);
  for (const d of later) {
    await loadDay(d.day);
    const idx = days.get(d.day);
    const hit = idx?.segments.find((s) => s.startMs > t && accept(s));
    if (hit) return hit;
  }
  return null;
}

export async function prevSegment(t: number, accept: (s: Seg) => boolean): Promise<Seg | null> {
  const earlier = (catalog.value?.days ?? []).filter((d) => d.from < t).reverse();
  for (const d of earlier) {
    await loadDay(d.day);
    const idx = days.get(d.day);
    const segs = idx?.segments ?? [];
    for (let i = segs.length - 1; i >= 0; i--) if (segs[i].startMs < t - 300 && accept(segs[i])) return segs[i];
  }
  return null;
}

// ---------- mixer ----------

export const trackKey = (channelId: number, speakerId: number) => `${channelId}:${speakerId}`;

export function trackStrip(key: string): Strip {
  return (ui.mix.tracks[key] ??= { mute: false, solo: false, gain: 1 });
}

export function channelStrip(id: number): Strip {
  return (ui.mix.channels[id] ??= { mute: false, solo: false, gain: 1 });
}

export function isVisible(channelId: number, speakerId: number): boolean {
  return !ui.hiddenChannels.has(channelId) && !ui.hiddenSpeakers.has(speakerId);
}

const anySolo = computed(
  () => Object.values(ui.mix.tracks).some((s) => s.solo) || Object.values(ui.mix.channels).some((s) => s.solo),
);

/** Effective gain of a track after filters, mute and solo (0 = silent). */
export function trackGain(channelId: number, speakerId: number): number {
  if (!isVisible(channelId, speakerId)) return 0;
  const t = ui.mix.tracks[trackKey(channelId, speakerId)];
  const c = ui.mix.channels[channelId];
  if (t?.mute || c?.mute) return 0;
  if (anySolo.value && !(t?.solo || c?.solo)) return 0;
  return (t?.gain ?? 1) * (c?.gain ?? 1);
}

export const isAudible = (s: Seg) => trackGain(s.channelId, s.speakerId) > 0;

// ---------- activity in the current scope ----------

/** Speech ms per channel / speaker among loaded segments in [from, to). */
export function activity(from: number, to: number) {
  const byChannel = new Map<number, number>();
  const bySpeaker = new Map<number, number>();
  for (const s of segmentsIn(from, to)) {
    const d = Math.min(s.endMs, to) - Math.max(s.startMs, from);
    byChannel.set(s.channelId, (byChannel.get(s.channelId) ?? 0) + d);
    bySpeaker.set(s.speakerId, (bySpeaker.get(s.speakerId) ?? 0) + d);
  }
  return { byChannel, bySpeaker };
}
