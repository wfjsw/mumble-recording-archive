import { computed, reactive, ref, shallowRef, watch } from 'vue';
import {
  fetchMeta,
  LogReader,
  LogReset,
  REC,
  setVersion,
  type Channel,
  type DayEntry,
  type Fields,
  type Meta,
  type Presence,
  type Rec,
  type Seg,
  type Speaker,
  type Totals,
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

export const meta = shallowRef<Meta | null>(null);
/** When new published data last arrived. */
export const loadedAt = ref(0);
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

// ---------- published data ----------

/** Resolution of the coarse per-day bitmaps used when zoomed out. */
export const BIT_MS = 60_000;

/**
 * A day's speech and presence at one-bit-per-minute, per track and per channel.
 * Zoomed-out views draw from these instead of walking every segment: a month of
 * dense speech is millions of clips but only a few million bits.
 */
export interface BitDay {
  from: number;
  to: number;
  minutes: number;
  speech: Map<string, Uint8Array>;
  presence: Map<string, Uint8Array>;
  byChannel: Map<number, Uint8Array>;
  /** Channel -> speakers heard in it, for list building. */
  tracks: Map<number, Set<number>>;
  /** Any channel, for the overview at coarse zoom. */
  all: Uint8Array;
  /** Speech ms per minute summed over all channels: how much voice, not just whether. */
  weight: Float32Array;
  /** Minutes with speech per track / channel / speaker, for whole-day totals. */
  trackMinutes: Map<string, number>;
  channelMinutes: Map<number, number>;
  speakerMinutes: Map<number, number>;
}

/** One loaded day log, applied incrementally. */
interface DayData {
  day: string;
  from: number;
  to: number;
  pack: string;
  reader: LogReader;
  /** Sorted by start. */
  segments: Seg[];
  segIndex: Map<number, Seg>;
  presence: Map<number, Presence>;
  uptime: Map<number, { startMs: number; endMs: number | null }>;
  speakers: Map<number, Speaker>;
  channels: Map<number, Channel>;
  /** Coarse bitmaps, rebuilt whenever the log grows. */
  bits: BitDay | null;
  /** Poll again on the next refresh (open day, or changed after we read it). */
  stale: boolean;
}

const days = new Map<string, DayData>();
const loadingDays = new Map<string, Promise<void>>();
const dayEntries = new Map<string, DayEntry>();
const liveChannels = new Map<number, Channel>();
let daysReader: LogReader | null = null;
let channelsReader: LogReader | null = null;
/** Longest stored segment; used to widen overlap lookups. */
const MAX_SEG_MS = 6 * MIN;

export const dayList = shallowRef<DayEntry[]>([]);

function resetData() {
  days.clear();
  loadingDays.clear();
  dayEntries.clear();
  liveChannels.clear();
  daysReader = channelsReader = null;
  dayList.value = [];
}

function applyDaysLog(recs: Rec[]) {
  for (const { type, f } of recs) {
    if (type === REC.DAY) {
      const day = f.str();
      const from = f.u();
      const to = f.u();
      if (!dayEntries.has(day)) dayEntries.set(day, { day, from, to, summary: null });
    } else if (type === REC.SUMMARY) {
      const e = dayEntries.get(f.str());
      if (!e) continue;
      e.summary = { segments: f.u(), totalMs: f.u(), bytes: f.u(), rawBytes: f.u(), firstMs: f.u(), lastMs: f.u() };
      markFinished(e);
      // A new summary after we read the day means the day log grew (late segments).
      const d = days.get(e.day);
      if (d) d.stale = true;
    } else if (type === REC.DAY_DELETED) {
      const day = f.str();
      dayEntries.delete(day);
      days.delete(day);
    }
  }
  if (recs.length) dayList.value = [...dayEntries.values()].sort((a, b) => a.from - b.from);
}

/** A finished day's files are immutable for a given summary: let them be cached. */
function markFinished(e: DayEntry) {
  const s = e.summary;
  if (!s) return;
  const v = `${s.segments}-${s.bytes}`;
  setVersion(`index/${e.day}.log`, v);
  const pack = days.get(e.day)?.pack;
  if (pack) setVersion(pack, v);
}

function readChannel(f: Fields): Channel {
  return { id: f.u(), mumbleId: f.u(), parentId: f.opt(), position: f.i(), name: f.str(), removed: false };
}

/** Returns the number of records read. */
async function pollChannels(m: Meta): Promise<number> {
  if (!channelsReader || channelsReader.rel !== m.channels) {
    channelsReader = new LogReader(m.channels);
    liveChannels.clear();
  }
  let n = 0;
  for (;;) {
    let next: number | null = null;
    const recs = await channelsReader.poll();
    n += recs.length;
    for (const { type, f } of recs) {
      if (type === REC.CHANNEL) {
        const c = readChannel(f);
        liveChannels.set(c.id, c);
      } else if (type === REC.CHANNEL_REMOVE) liveChannels.delete(f.u());
      else if (type === REC.NEXT) next = f.u();
    }
    if (next === null) return n;
    // Compacted: continue with the new generation from scratch.
    channelsReader = new LogReader(`channels.${next}.log`);
    liveChannels.clear();
  }
}

// ---------- coarse bitmaps ----------

const blank = (minutes: number) => new Uint8Array(((minutes + 7) >> 3) || 1);

function pop8(v: number) {
  v -= (v >> 1) & 0x55;
  v = (v & 0x33) + ((v >> 2) & 0x33);
  return (v + (v >> 4)) & 0x0f;
}

function popArr(arr: Uint8Array): number {
  let n = 0;
  for (let i = 0; i < arr.length; i++) n += pop8(arr[i]);
  return n;
}

/** Set every bit covering [a, b) (whole bytes at a time). */
function setBits(arr: Uint8Array, minutes: number, from: number, a: number, b: number) {
  let i = Math.floor((a - from) / BIT_MS);
  let j = Math.ceil((b - from) / BIT_MS) - 1;
  if (i < 0) i = 0;
  if (j > minutes - 1) j = minutes - 1;
  if (j < i) return;
  while (i <= j && (i & 7)) {
    arr[i >> 3] |= 1 << (i & 7);
    i++;
  }
  const endByte = (j + 1) >> 3;
  if (endByte > i >> 3) arr.fill(0xff, i >> 3, endByte);
  i = Math.max(i, endByte << 3);
  while (i <= j) {
    arr[i >> 3] |= 1 << (i & 7);
    i++;
  }
}

function buildBits(d: DayData) {
  const minutes = Math.max(1, Math.ceil((d.to - d.from) / BIT_MS));
  const b: BitDay = {
    from: d.from,
    to: d.to,
    minutes,
    speech: new Map(),
    presence: new Map(),
    byChannel: new Map(),
    tracks: new Map(),
    all: blank(minutes),
    weight: new Float32Array(minutes),
    trackMinutes: new Map(),
    channelMinutes: new Map(),
    speakerMinutes: new Map(),
  };
  for (const s of d.segments) {
    const a = Math.max(s.startMs, d.from);
    const z = Math.min(s.endMs, d.to);
    if (z <= a) continue;
    const k = trackKey(s.channelId, s.speakerId);
    let arr = b.speech.get(k);
    if (!arr) b.speech.set(k, (arr = blank(minutes)));
    setBits(arr, minutes, d.from, a, z);
    let ch = b.byChannel.get(s.channelId);
    if (!ch) b.byChannel.set(s.channelId, (ch = blank(minutes)));
    setBits(ch, minutes, d.from, a, z);
    setBits(b.all, minutes, d.from, a, z);
    // total speech time per minute (all channels): the overview's density
    let mi = Math.max(0, Math.floor((a - d.from) / BIT_MS));
    const mj = Math.min(minutes - 1, Math.ceil((z - d.from) / BIT_MS) - 1);
    for (let m = mi; m <= mj; m++) {
      const ms = Math.max(0, Math.min(z, d.from + (m + 1) * BIT_MS) - Math.max(a, d.from + m * BIT_MS));
      b.weight[m] += ms;
    }
    let sps = b.tracks.get(s.channelId);
    if (!sps) b.tracks.set(s.channelId, (sps = new Set()));
    sps.add(s.speakerId);
  }
  for (const p of d.presence.values()) {
    const a = Math.max(p.startMs, d.from);
    const z = Math.min(p.endMs ?? d.to, d.to);
    if (z <= a) continue;
    const k = trackKey(p.channelId, p.speakerId);
    let arr = b.presence.get(k);
    if (!arr) b.presence.set(k, (arr = blank(minutes)));
    setBits(arr, minutes, d.from, a, z);
  }
  // Totals for whole-day scopes (the sidebars), so they never scan bits.
  for (const [k, arr] of b.speech) {
    const n = popArr(arr);
    b.trackMinutes.set(k, n);
    const sp = Number(k.slice(k.indexOf(':') + 1));
    b.speakerMinutes.set(sp, (b.speakerMinutes.get(sp) ?? 0) + n);
  }
  for (const [ch, arr] of b.byChannel) b.channelMinutes.set(ch, popArr(arr));
  d.bits = b;
}

/** Any bit set in [a, b). */
export function anyBit(arr: Uint8Array | undefined, day: BitDay, a: number, b: number): boolean {
  if (!arr) return false;
  let i = Math.floor((a - day.from) / BIT_MS);
  let j = Math.ceil((b - day.from) / BIT_MS) - 1;
  if (i < 0) i = 0;
  if (j > day.minutes - 1) j = day.minutes - 1;
  if (j < i) return false;
  while (i <= j) {
    if ((i & 7) === 0 && i + 8 <= j + 1) {
      if (arr[i >> 3]) return true;
      i += 8;
    } else {
      if (arr[i >> 3] & (1 << (i & 7))) return true;
      i++;
    }
  }
  return false;
}

/** Total speech ms (all channels) in [a, b). */
export function weightIn(d: BitDay, a: number, b: number): number {
  let i = Math.floor((a - d.from) / BIT_MS);
  let j = Math.ceil((b - d.from) / BIT_MS) - 1;
  if (i < 0) i = 0;
  if (j > d.minutes - 1) j = d.minutes - 1;
  let n = 0;
  for (let m = i; m <= j; m++) n += d.weight[m];
  return n;
}

/** Number of set bits (minutes with activity) in [a, b). */
export function countBits(arr: Uint8Array, d: BitDay, a: number, b: number): number {
  let i = Math.floor((a - d.from) / BIT_MS);
  let j = Math.ceil((b - d.from) / BIT_MS) - 1;
  if (i < 0) i = 0;
  if (j > d.minutes - 1) j = d.minutes - 1;
  if (j < i) return 0;
  let n = 0;
  while (i <= j && (i & 7)) {
    if (arr[i >> 3] & (1 << (i & 7))) n++;
    i++;
  }
  const endByte = (j + 1) >> 3;
  for (let k = i >> 3; k < endByte; k++) n += pop8(arr[k]);
  i = Math.max(i, endByte << 3);
  while (i <= j) {
    if (arr[i >> 3] & (1 << (i & 7))) n++;
    i++;
  }
  return n;
}

/** Coarse bitmaps of the loaded days overlapping [from, to). */
export function bitDaysIn(from: number, to: number): BitDay[] {
  const out: BitDay[] = [];
  for (const d of loadedDays(from, to)) if (d.bits) out.push(d.bits);
  return out;
}

/** Channels with speech in [from, to) and the speakers heard in them. */
export function activeTracks(from: number, to: number): Map<number, Set<number>> {
  const out = new Map<number, Set<number>>();
  for (const d of loadedDays(from, to)) {
    if (!d.bits) continue;
    for (const [ch, sps] of d.bits.tracks) {
      let set = out.get(ch);
      if (!set) out.set(ch, (set = new Set()));
      for (const s of sps) set.add(s);
    }
  }
  return out;
}

function insertSeg(d: DayData, s: Seg) {
  const old = d.segIndex.get(s.id);
  if (old) d.segments.splice(d.segments.indexOf(old), 1);
  d.segIndex.set(s.id, s);
  const a = d.segments;
  if (!a.length || a[a.length - 1].startMs <= s.startMs) {
    a.push(s);
    return;
  }
  let lo = 0;
  let hi = a.length;
  while (lo < hi) {
    const m = (lo + hi) >> 1;
    if (a[m].startMs <= s.startMs) lo = m + 1;
    else hi = m;
  }
  a.splice(lo, 0, s);
}

function applyDayLog(d: DayData, recs: Rec[]) {
  for (const { type, f } of recs) {
    switch (type) {
      case REC.HEADER: {
        f.u(); // kind
        f.str(); // day
        d.from = f.u();
        d.to = f.u();
        d.pack = f.str();
        const e = dayEntries.get(d.day);
        if (e) markFinished(e);
        break;
      }
      case REC.SPEAKER: {
        const id = f.u();
        const userId = f.opt();
        d.speakers.set(id, { id, userId, name: f.str() });
        break;
      }
      case REC.CHANNEL: {
        const c = readChannel(f);
        d.channels.set(c.id, c);
        break;
      }
      case REC.SEGMENT: {
        const id = f.u();
        const speakerId = f.u();
        const channelId = f.u();
        const startMs = d.from + f.i();
        const endMs = startMs + f.u();
        const target = f.u();
        const status = f.u() ? 'error' : 'ready';
        const path = f.str() || d.pack;
        const offset = f.opt();
        const size = f.u();
        const rawSize = f.u();
        const peaks = f.bytes().slice();
        insertSeg(d, { id, speakerId, channelId, startMs, endMs, target, status, path, offset, size, rawSize, peaks });
        break;
      }
      case REC.PRESENCE_OPEN: {
        const id = f.u();
        const speakerId = f.u();
        const channelId = f.u();
        const startMs = d.from + f.i();
        if (!d.presence.has(id)) d.presence.set(id, { id, speakerId, channelId, startMs, endMs: null });
        break;
      }
      case REC.PRESENCE_CLOSE: {
        const p = d.presence.get(f.u());
        if (p) p.endMs = d.from + f.i();
        break;
      }
      case REC.UPTIME_OPEN: {
        const id = f.u();
        const startMs = d.from + f.i();
        if (!d.uptime.has(id)) d.uptime.set(id, { startMs, endMs: null });
        break;
      }
      case REC.UPTIME_CLOSE: {
        const u = d.uptime.get(f.u());
        if (u) u.endMs = d.from + f.i();
        break;
      }
    }
  }
}

/** Bumped whenever a day log starts or finishes loading. */
export const loadingVersion = ref(0);

export interface PendingDay {
  day: string;
  from: number;
  to: number;
}

/**
 * Days the catalog says have data in [from, to) whose log is not loaded yet.
 * Lets the timeline say "loading" instead of looking empty.
 */
export function pendingIn(from: number, to: number): PendingDay[] {
  void loadingVersion.value;
  const out: PendingDay[] = [];
  for (const e of entriesIn(from, to)) if (!days.has(e.day)) out.push({ day: e.day, from: e.from, to: e.to });
  return out;
}

/** Returns the number of records read. */
async function pollDay(d: DayData): Promise<number> {
  const recs = await d.reader.poll();
  applyDayLog(d, recs);
  if (recs.length || !d.bits) buildBits(d);
  // Today (or a day still waiting for its totals) keeps changing.
  d.stale = !dayEntries.get(d.day)?.summary || d.to > Date.now();
  return recs.length;
}

/** Names known from the loaded days (later days win) and the live channel tree. */
function rebuildNames() {
  const sp = new Map<number, Speaker>();
  const ch = new Map<number, Channel>();
  for (const d of [...days.values()].sort((a, b) => a.from - b.from)) {
    for (const s of d.speakers.values()) sp.set(s.id, s);
    for (const c of d.channels.values()) ch.set(c.id, { ...c, removed: true });
  }
  for (const c of liveChannels.values()) ch.set(c.id, c);
  speakerList.value = [...sp.values()];
  channelList.value = [...ch.values()];
}

/** How often the UI polls for new data (the bot publishes every few seconds). */
const POLL_MS = 10_000;
let inflight: Promise<void> | null = null;

/** Fetch whatever was published since the last refresh (cheap when nothing changed). */
export function refreshCatalog(quiet = false): Promise<void> {
  inflight ??= refresh(true, quiet).finally(() => (inflight = null));
  return inflight;
}

/** Poll for new data in the background while the page is visible. */
export function startAutoRefresh() {
  setInterval(() => {
    if (!document.hidden) void refreshCatalog(true);
  }, POLL_MS);
  // Catch up right away when the tab comes back.
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) void refreshCatalog(true);
  });
}

async function refresh(retry: boolean, quiet: boolean): Promise<void> {
  try {
    if (!quiet) ui.loading = true;
    const m = await fetchMeta();
    if (meta.value && meta.value.generation !== m.generation) resetData();
    meta.value = m;
    daysReader ??= new LogReader(m.days);
    const dayRecs = await daysReader.poll();
    applyDaysLog(dayRecs);
    let changed = dayRecs.length + (await pollChannels(m));
    for (const n of await Promise.all([...days.values()].filter((d) => d.stale).map(pollDay))) changed += n;
    // Redraw only when something arrived (polls run every few seconds).
    if (changed) {
      loadedAt.value = Date.now();
      rebuildNames();
      dataVersion.value++;
    }
    ui.error = '';
  } catch (e) {
    if (e instanceof LogReset && retry) {
      // A log was rebuilt or compacted under us: start over.
      resetData();
      return refresh(false, quiet);
    }
    ui.error = `Cannot load archive index: ${(e as Error).message}`;
  } finally {
    ui.loading = false;
  }
}

function entriesIn(from: number, to: number): DayEntry[] {
  return dayList.value.filter((d) => d.to > from - MAX_SEG_MS && d.from < to);
}

function loadDay(day: string): Promise<void> {
  if (days.has(day)) return Promise.resolve();
  const existing = loadingDays.get(day);
  if (existing) return existing;
  const e = dayEntries.get(day);
  if (!e) return Promise.resolve();
  const d: DayData = {
    day,
    from: e.from,
    to: e.to,
    pack: '',
    reader: new LogReader(`index/${day}.log`),
    segments: [],
    segIndex: new Map(),
    presence: new Map(),
    uptime: new Map(),
    speakers: new Map(),
    channels: new Map(),
    bits: null,
    stale: true,
  };
  const p = pollDay(d)
    .then(() => {
      days.set(day, d);
      rebuildNames();
      dataVersion.value++;
    })
    .catch((err) => console.warn('day log', day, err))
    .finally(() => {
      loadingDays.delete(day);
      loadingVersion.value++;
    });
  loadingDays.set(day, p);
  loadingVersion.value++;
  return p;
}

/** Make sure all day logs overlapping [from, to) are loaded. */
export function ensureRange(from: number, to: number): Promise<void> {
  return Promise.all(entriesIn(from, to).map((d) => loadDay(d.day))).then(() => {});
}

function loadedDays(from: number, to: number): DayData[] {
  const out: DayData[] = [];
  for (const d of days.values()) if (d.to > from - MAX_SEG_MS && d.from < to) out.push(d);
  return out.sort((a, b) => a.from - b.from);
}

/**
 * One-entry cache for the last window asked for. The editor, both sidebars and
 * the playback engine all query overlapping ranges in the same tick, and at a
 * day's zoom that is hundreds of thousands of segments: never scan twice.
 * Callers must treat the result as read-only.
 */
let segCache: { key: string; value: Seg[] } | null = null;

/** Loaded segments overlapping [from, to), ordered by start. */
export function segmentsIn(from: number, to: number): Seg[] {
  const key = `${from}:${to}:${dataVersion.value}`;
  if (segCache?.key === key) return segCache.value;
  const value = scanSegments(from, to);
  segCache = { key, value };
  return value;
}

function scanSegments(from: number, to: number): Seg[] {
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

/**
 * An interval left open in a finished day's log lasts until that day's end;
 * the next day's log carries it on. Merge the copies, keeping the longest.
 */
function mergeIntervals<T extends { startMs: number; endMs: number | null }>(
  from: number,
  to: number,
  pick: (d: DayData) => Iterable<[number, T]>,
): T[] {
  const now = Date.now();
  const best = new Map<number, T>();
  for (const d of loadedDays(from, to)) {
    for (const [id, x] of pick(d)) {
      const end = x.endMs ?? (d.to <= now ? d.to : null);
      const v = { ...x, endMs: end };
      const prev = best.get(id);
      if (!prev || (prev.endMs !== null && (end === null || end > prev.endMs))) best.set(id, v);
    }
  }
  return [...best.values()].filter((x) => x.startMs < to && (x.endMs === null || x.endMs > from));
}

export function presenceIn(from: number, to: number): Presence[] {
  return mergeIntervals(from, to, (d) => d.presence);
}

export function uptimeIn(from: number, to: number): [number, number][] {
  const now = Date.now();
  return mergeIntervals(from, to, (d) => d.uptime)
    .map((u): [number, number] => [u.startMs, u.endMs ?? now])
    .sort((a, b) => a[0] - b[0]);
}

/** First audible segment starting after t, loading further days as needed. */
export async function nextSegment(t: number, accept: (s: Seg) => boolean): Promise<Seg | null> {
  for (const e of dayList.value.filter((d) => d.to > t)) {
    await loadDay(e.day);
    const hit = days.get(e.day)?.segments.find((s) => s.startMs > t && accept(s));
    if (hit) return hit;
  }
  return null;
}

export async function prevSegment(t: number, accept: (s: Seg) => boolean): Promise<Seg | null> {
  for (const e of dayList.value.filter((d) => d.from < t).reverse()) {
    await loadDay(e.day);
    const segs = days.get(e.day)?.segments ?? [];
    for (let i = segs.length - 1; i >= 0; i--) if (segs[i].startMs < t - 300 && accept(segs[i])) return segs[i];
  }
  return null;
}

/** Totals of a day: published summary, or computed from its loaded log. */
export function dayTotals(e: DayEntry): Totals | null {
  if (e.summary) return e.summary;
  const d = days.get(e.day);
  if (!d) return null;
  const t: Totals = { segments: 0, totalMs: 0, bytes: 0, rawBytes: 0, firstMs: 0, lastMs: 0 };
  for (const s of d.segments) {
    t.segments++;
    t.totalMs += s.endMs - s.startMs;
    t.bytes += s.size;
    t.rawBytes += s.rawSize;
    t.firstMs = t.firstMs ? Math.min(t.firstMs, s.startMs) : s.startMs;
    t.lastMs = Math.max(t.lastMs, s.endMs);
  }
  return t;
}

export interface CatalogDay extends DayEntry {
  segments: number | null;
  totalMs: number | null;
}

/** Archive overview (server info, totals, days), derived from the logs. */
export const catalog = computed(() => {
  void dataVersion.value;
  const m = meta.value;
  if (!m) return null;
  const stats = { segments: 0, totalMs: 0, bytes: 0, rawBytes: 0, earliestMs: null as number | null, latestMs: null as number | null };
  const list: CatalogDay[] = dayList.value.map((e) => {
    const t = dayTotals(e);
    if (t && t.segments) {
      stats.segments += t.segments;
      stats.totalMs += t.totalMs;
      stats.bytes += t.bytes;
      stats.rawBytes += t.rawBytes;
      stats.earliestMs = stats.earliestMs === null ? t.firstMs : Math.min(stats.earliestMs, t.firstMs);
      stats.latestMs = Math.max(stats.latestMs ?? 0, t.lastMs);
    }
    return { ...e, segments: t?.segments ?? null, totalMs: t?.totalMs ?? null };
  });
  return { server: m.server, bot: m.bot, bitrate: m.bitrate, peaksPerSec: m.peaksPerSec, generatedAt: loadedAt.value, stats, days: list };
});

// ---------- mixer ----------

export const trackKey = (channelId: number, speakerId: number) => `${channelId}:${speakerId}`;

const DEFAULT_STRIP: Strip = Object.freeze({ mute: false, solo: false, gain: 1 });

/**
 * Mixer strips are only created when the user actually changes one. Reading
 * must not materialise them: the editor looks strips up while drawing, and a
 * record per track ever displayed would grow without bound.
 */
export function trackStrip(key: string): Strip {
  return ui.mix.tracks[key] ?? DEFAULT_STRIP;
}

export function channelStrip(id: number): Strip {
  return ui.mix.channels[id] ?? DEFAULT_STRIP;
}

export function setTrack(key: string, patch: Partial<Strip>) {
  Object.assign((ui.mix.tracks[key] ??= { ...DEFAULT_STRIP }), patch);
}

export function setChannel(id: number, patch: Partial<Strip>) {
  Object.assign((ui.mix.channels[id] ??= { ...DEFAULT_STRIP }), patch);
}

export function toggleTrack(key: string, field: 'mute' | 'solo') {
  setTrack(key, { [field]: !trackStrip(key)[field] });
}

export function toggleChannel(id: number, field: 'mute' | 'solo') {
  setChannel(id, { [field]: !channelStrip(id)[field] });
}

/**
 * Bumped whenever the mixer changes. Drawing watches this instead of
 * `JSON.stringify(ui.mix)`, which ran on every playhead tick.
 */
export const mixVersion = ref(0);
watch(ui.mix, () => mixVersion.value++, { deep: true });

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

export interface Activity {
  byChannel: Map<number, number>;
  bySpeaker: Map<number, number>;
}

/** Cached like segmentsIn: the channel tree and the speaker list ask for the same window. */
let activityCache: { key: string; value: Activity } | null = null;

/**
 * Speech ms per channel / speaker in [from, to). Whole-day ranges -- what the
 * sidebars ask for -- are answered from the coarse bitmaps' minute totals, so
 * a month-wide scope costs a few map merges instead of a segment scan.
 */
export function activity(from: number, to: number): Activity {
  const key = `${from}:${to}:${dataVersion.value}`;
  if (activityCache?.key === key) return activityCache.value;
  const days = loadedDays(from, to);
  const byChannel = new Map<number, number>();
  const bySpeaker = new Map<number, number>();
  for (const d of days) {
    const b = d.bits;
    if (!b) continue;
    const whole = from <= b.from && to >= b.to;
    for (const [ch, arr] of b.byChannel) {
      const n = whole ? (b.channelMinutes.get(ch) ?? 0) : countBits(arr, b, from, to);
      if (n) byChannel.set(ch, (byChannel.get(ch) ?? 0) + n * BIT_MS);
    }
    if (whole) {
      for (const [sp, n] of b.speakerMinutes) bySpeaker.set(sp, (bySpeaker.get(sp) ?? 0) + n * BIT_MS);
    } else {
      for (const [k, arr] of b.speech) {
        const n = countBits(arr, b, from, to);
        if (!n) continue;
        const sp = Number(k.slice(k.indexOf(':') + 1));
        bySpeaker.set(sp, (bySpeaker.get(sp) ?? 0) + n * BIT_MS);
      }
    }
  }
  const value = { byChannel, bySpeaker };
  activityCache = { key, value };
  return value;
}
