// Fetch + decode segment audio in the browser: pack records are read with
// HTTP Range requests (adjacent records merged into one request) and decoded
// by a small pool of Web Workers running a wasm libopus build, so decoding
// never blocks drawing or playback scheduling.

import { fileUrl, type Seg } from './api';
import { Decoder, fetchRangeUrl, fetchUrl, groupRanges, parse, RATE as CODEC_RATE } from './codec';

export const RATE = CODEC_RATE;

type Kind = 'record' | 'ogg';

interface Job {
  resolve: (pcm: Float32Array) => void;
  reject: (e: Error) => void;
}

/** Decoder workers; each takes the job with the fewest pending. */
class DecodePool {
  private workers: { w: Worker; pending: number }[] = [];
  private jobs = new Map<number, Job & { worker: number }>();
  private next = 1;
  private fallback: Decoder | null = null;

  constructor(size: number) {
    if (typeof Worker === 'undefined') return;
    for (let i = 0; i < size; i++) {
      const w = new Worker(new URL('./decode.worker.ts', import.meta.url), { type: 'module' });
      w.onmessage = (e: MessageEvent<{ id: number; pcm?: Float32Array; error?: string }>) => {
        const job = this.jobs.get(e.data.id);
        if (!job) return;
        this.jobs.delete(e.data.id);
        this.workers[job.worker].pending--;
        if (e.data.pcm) job.resolve(e.data.pcm);
        else job.reject(new Error(e.data.error ?? 'decode failed'));
      };
      this.workers.push({ w, pending: 0 });
    }
  }

  decode(kind: Kind, bytes: Uint8Array): Promise<Float32Array> {
    if (!this.workers.length) {
      this.fallback ??= new Decoder();
      return this.fallback.decode(parse(kind, bytes));
    }
    let best = 0;
    this.workers.forEach((x, i) => {
      if (x.pending < this.workers[best].pending) best = i;
    });
    const id = this.next++;
    // Own copy of the bytes, transferred to the worker.
    const own = bytes.slice();
    return new Promise((resolve, reject) => {
      this.jobs.set(id, { resolve, reject, worker: best });
      this.workers[best].pending++;
      this.workers[best].w.postMessage({ id, kind, bytes: own }, [own.buffer]);
    });
  }
}

const pool = new DecodePool(Math.min(4, Math.max(1, Math.floor((navigator.hardwareConcurrency || 4) / 2))));

// ---------- cache ----------

const MAX_CACHED_SAMPLES = 15 * 60 * RATE; // ~170 MB of f32
const cache = new Map<number, Float32Array>(); // insertion order = LRU
let cachedSamples = 0;
const inflight = new Map<number, Promise<Float32Array | null>>();

export function cached(id: number): Float32Array | undefined {
  const v = cache.get(id);
  if (v) {
    cache.delete(id);
    cache.set(id, v);
  }
  return v;
}

function remember(id: number, pcm: Float32Array) {
  cache.set(id, pcm);
  cachedSamples += pcm.length;
  for (const [k, v] of cache) {
    if (cachedSamples <= MAX_CACHED_SAMPLES) break;
    cache.delete(k);
    cachedSamples -= v.length;
  }
}

// ---------- batched loading ----------

let queue: { seg: Seg; resolve: (v: Float32Array | null) => void }[] = [];
let flushScheduled = false;

/** Decoded mono 48 kHz audio for a segment (null if unavailable). */
export function load(seg: Seg): Promise<Float32Array | null> {
  const c = cached(seg.id);
  if (c) return Promise.resolve(c);
  let p = inflight.get(seg.id);
  if (!p) {
    p = new Promise((resolve) => {
      queue.push({ seg, resolve });
      if (!flushScheduled) {
        flushScheduled = true;
        setTimeout(flush, 8);
      }
    });
    inflight.set(seg.id, p);
    p.finally(() => inflight.delete(seg.id));
  }
  return p;
}

async function finish(seg: Seg, kind: Kind, bytes: Uint8Array, resolve: (v: Float32Array | null) => void) {
  try {
    const pcm = await pool.decode(kind, bytes);
    remember(seg.id, pcm);
    resolve(pcm);
  } catch (e) {
    console.warn('decode failed', seg.id, e);
    resolve(null);
  }
}

async function flush() {
  flushScheduled = false;
  const items = queue;
  queue = [];
  const byPath = new Map<string, typeof items>();
  for (const it of items) {
    if (it.seg.offset === null) {
      // standalone Ogg file
      const u = fileUrl(it.seg.path, false);
      fetchUrl(u.href, u.cache)
        .then((b) => finish(it.seg, 'ogg', b, it.resolve))
        .catch(() => it.resolve(null));
      continue;
    }
    const l = byPath.get(it.seg.path) ?? [];
    l.push(it);
    byPath.set(it.seg.path, l);
  }
  for (const [path, list] of byPath) {
    const u = fileUrl(path, true);
    const ranges = list.map((it) => ({ offset: it.seg.offset!, size: it.seg.size, it })).sort((a, b) => a.offset - b.offset);
    for (const g of groupRanges(ranges)) {
      fetchRangeUrl(u.href, g.start, g.end, u.cache)
        .then((buf) => {
          for (const { offset, size, it } of g.items) finish(it.seg, 'record', buf.subarray(offset - g.start, offset - g.start + size), it.resolve);
        })
        .catch((e) => {
          console.warn('fetch failed', path, e);
          for (const { it } of g.items) it.resolve(null);
        });
    }
  }
}
