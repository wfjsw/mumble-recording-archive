// Static data layer: everything is read from files the bot publishes.
//
//   meta.json                 server info + current log file names
//   days.log                  days with data, per-day totals once a day is over
//   channels.<gen>.log        live channel tree
//   index/<day>.log           one day: names, segments, presence, bot sessions
//   audio/<month>/<day>.pack  audio, read with HTTP Range
//
// The .log files are append-only (see src/publog.rs in the bot): after the first
// read, a refresh asks only for the bytes past what we already have
// (`Range: bytes=<length>-`), so refreshing costs nothing when nothing changed.

export interface Seg {
  id: number;
  speakerId: number;
  channelId: number;
  startMs: number;
  endMs: number;
  /** 0 normal, 1 shout, 2 whisper, 3 heard via channel listener, 31 loopback */
  target: number;
  status: 'ready' | 'error';
  path: string;
  offset: number | null;
  size: number;
  rawSize: number;
  peaks: Uint8Array | null;
}

export interface Speaker {
  id: number;
  name: string;
  userId: number | null;
}

export interface Channel {
  id: number;
  mumbleId: number;
  parentId: number | null;
  name: string;
  position: number;
  /** Not on the server any more (only known from day logs). */
  removed: boolean;
}

export interface Presence {
  id: number;
  speakerId: number;
  channelId: number;
  startMs: number;
  endMs: number | null;
}

export interface Totals {
  segments: number;
  totalMs: number;
  bytes: number;
  rawBytes: number;
  firstMs: number;
  lastMs: number;
}

export interface DayEntry {
  day: string;
  from: number;
  to: number;
  /** Published once the day is over; until then totals come from the loaded day log. */
  summary: Totals | null;
}

export interface Meta {
  format: number;
  generation: string;
  server: string;
  bot: string;
  bitrate: string;
  peaksPerSec: number;
  days: string;
  channels: string;
}

/** Base URL of the bot's public dir; override with ./config.json {"dataUrl": "..."} */
let dataUrl = 'data/';

export function dataHref(rel: string): string {
  const url = new URL(rel, new URL(dataUrl, location.href));
  const v = versions.get(rel);
  if (v) url.searchParams.set('v', v);
  return url.href;
}

/**
 * Files of finished days no longer change (a late clip publishes a new day
 * summary, i.e. a new version). They are fetched as `<file>?v=<version>`,
 * which the web server may cache for a long time; everything else is live and
 * revalidated or bypasses the cache.
 */
const versions = new Map<string, string>();

export function setVersion(rel: string, v: string) {
  versions.set(rel, v);
}

/**
 * Cache mode for a published file: versioned (finished) files use the normal
 * HTTP cache; a live file is revalidated when read whole (cheap 304) and its
 * tail reads (Range) bypass the cache.
 */
const cacheMode = (rel: string, range: boolean): RequestCache =>
  versions.has(rel) ? 'default' : range ? 'no-store' : 'no-cache';

export async function loadConfig() {
  try {
    const res = await fetch('config.json', { cache: 'no-cache' });
    if (res.ok) {
      const c = await res.json();
      if (typeof c.dataUrl === 'string') dataUrl = c.dataUrl.endsWith('/') ? c.dataUrl : `${c.dataUrl}/`;
    }
  } catch {}
}

export async function fetchMeta(): Promise<Meta> {
  // no-cache: revalidate (a 304 almost always; meta.json changes only on restart/compaction)
  const res = await fetch(dataHref('meta.json'), { cache: 'no-cache' });
  if (!res.ok) throw new Error(`meta.json: ${res.status} ${res.statusText}`);
  return res.json();
}

// ---------- append-only logs ----------

export const REC = {
  HEADER: 0,
  DAY: 1,
  SUMMARY: 2,
  DAY_DELETED: 3,
  CHANNEL: 4,
  CHANNEL_REMOVE: 5,
  NEXT: 6,
  SPEAKER: 7,
  SEGMENT: 8,
  PRESENCE_OPEN: 9,
  PRESENCE_CLOSE: 10,
  UPTIME_OPEN: 11,
  UPTIME_CLOSE: 12,
} as const;

const MAGIC = [0x4d, 0x41, 0x4c, 0x31]; // "MAL1"
const utf8 = new TextDecoder();

/** Field reader for one record (LEB128 varints, zigzag for signed values). */
export class Fields {
  constructor(
    private b: Uint8Array,
    public pos: number,
  ) {}
  u(): number {
    let v = 0;
    let mul = 1;
    for (;;) {
      const x = this.b[this.pos++];
      if (x === undefined) throw new Error('truncated record');
      v += (x & 0x7f) * mul; // multiply, not shift: values exceed 32 bits
      if (x < 0x80) return v;
      mul *= 128;
    }
  }
  i(): number {
    const v = this.u();
    return v % 2 ? -(v + 1) / 2 : v / 2;
  }
  opt(): number | null {
    const v = this.u();
    return v === 0 ? null : v - 1;
  }
  bytes(): Uint8Array {
    const n = this.u();
    const out = this.b.subarray(this.pos, this.pos + n);
    this.pos += n;
    return out;
  }
  str(): string {
    return utf8.decode(this.bytes());
  }
}

export interface Rec {
  type: number;
  f: Fields;
}

/** Thrown when a log was replaced or truncated: the reader must start over. */
export class LogReset extends Error {}

/**
 * Incremental reader of one append-only log. `poll()` returns the records
 * added since the previous call; an incomplete trailing record (still being
 * written) is left for the next poll.
 */
export class LogReader {
  offset = 0;
  constructor(public rel: string) {}

  async poll(): Promise<Rec[]> {
    const start = this.offset;
    const headers: HeadersInit = start ? { Range: `bytes=${start}-` } : {};
    const res = await fetch(dataHref(this.rel), { headers, cache: cacheMode(this.rel, start > 0) });
    if (res.status === 416) {
      // Nothing past `start`; a total length below it means the file shrank.
      const total = Number(res.headers.get('Content-Range')?.split('/')[1]);
      if (total < start) throw new LogReset(this.rel);
      return [];
    }
    if (res.status === 404 && start) throw new LogReset(this.rel);
    if (!res.ok) throw new Error(`${this.rel}: ${res.status} ${res.statusText}`);
    let buf = new Uint8Array(await res.arrayBuffer());
    // A server that ignores Range answers 200 with the whole file.
    if (res.status === 200 && start) {
      if (buf.length < start) throw new LogReset(this.rel);
      buf = buf.subarray(start);
    }
    let pos = 0;
    if (start === 0) {
      if (buf.length < 4 || MAGIC.some((m, i) => buf[i] !== m)) throw new Error(`${this.rel}: not a published log`);
      pos = 4;
    }
    const out: Rec[] = [];
    for (;;) {
      const f = new Fields(buf, pos);
      let len: number;
      try {
        len = f.u();
      } catch {
        break;
      }
      const end = f.pos + len;
      if (len === 0 || end > buf.length) break;
      // Each record gets its own view so a reader cannot run into the next one.
      out.push({ type: buf[f.pos], f: new Fields(buf.subarray(0, end), f.pos + 1) });
      pos = end;
    }
    this.offset = start + pos;
    return out;
  }
}

/** URL and cache mode for fetching a published file (for workers, which lack this module's state). */
export function fileUrl(rel: string, range: boolean): { href: string; cache: RequestCache } {
  return { href: dataHref(rel), cache: cacheMode(rel, range) };
}
