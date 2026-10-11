// Fetch + decode segment audio entirely in the browser:
// pack records are read with HTTP Range requests (adjacent records merged into
// one request) and the Opus packets are decoded with a wasm libopus build.

import { OpusDecoder } from 'opus-decoder';
import { fetchFile, fetchRange, type Seg } from './api';

export const RATE = 48000;

interface Parsed {
  preSkip: number;
  samples: number;
  packets: Uint8Array[];
}

function varint(buf: Uint8Array, pos: { p: number }): number {
  let v = 0;
  let mul = 1;
  for (;;) {
    const b = buf[pos.p++];
    if (b === undefined) throw new Error('truncated record');
    v += (b & 0x7f) * mul;
    if (!(b & 0x80)) return v;
    mul *= 128;
  }
}

/** Parse a "MAR1" pack record (see src/pack.rs). */
export function parseRecord(buf: Uint8Array): Parsed {
  if (buf[0] !== 0x4d || buf[1] !== 0x41 || buf[2] !== 0x52 || buf[3] !== 0x31) throw new Error('bad record magic');
  const len = buf[4] | (buf[5] << 8) | (buf[6] << 16) | (buf[7] << 24);
  const body = buf.subarray(8, 8 + len);
  const pos = { p: 0 };
  varint(body, pos); // seg id
  varint(body, pos); // start ms
  varint(body, pos); // speaker
  varint(body, pos); // channel
  const samples = varint(body, pos);
  const preSkip = varint(body, pos);
  const n = varint(body, pos);
  const packets: Uint8Array[] = [];
  for (let i = 0; i < n; i++) {
    const l = varint(body, pos);
    packets.push(body.subarray(pos.p, pos.p + l));
    pos.p += l;
  }
  return { preSkip, samples, packets };
}

/** Demux a standalone Ogg Opus file (used for segments kept as pass-through). */
export function parseOgg(data: Uint8Array): Parsed {
  const packets: Uint8Array[] = [];
  let cur: number[] = [];
  let pos = 0;
  let granule = 0;
  while (pos + 27 <= data.length && data[pos] === 0x4f && data[pos + 1] === 0x67) {
    const n = data[pos + 26];
    const lacing = data.subarray(pos + 27, pos + 27 + n);
    let off = pos + 27 + n;
    const g = new DataView(data.buffer, data.byteOffset + pos + 6, 8).getBigInt64(0, true);
    if (g > 0n) granule = Number(g);
    for (const l of lacing) {
      for (let i = 0; i < l; i++) cur.push(data[off + i]);
      off += l;
      if (l < 255) {
        packets.push(Uint8Array.from(cur));
        cur = [];
      }
    }
    pos = off;
  }
  const head = packets[0];
  const preSkip = head && head.length >= 12 ? head[10] | (head[11] << 8) : 0;
  return { preSkip, samples: Math.max(0, granule - preSkip), packets: packets.slice(2) };
}

let decoder: OpusDecoder | null = null;
let decoderReady: Promise<void> | null = null;
let decodeChain: Promise<unknown> = Promise.resolve();

async function decodePackets(p: Parsed): Promise<Float32Array> {
  if (!decoder) {
    decoder = new OpusDecoder({ channels: 1, preSkip: 0 });
    decoderReady = decoder.ready;
  }
  await decoderReady;
  // One decoder, serialized; reset between independent streams.
  const job = decodeChain.then(async () => {
    await decoder!.reset();
    const out = decoder!.decodeFrames(p.packets);
    const pcm = out.channelData[0] ?? new Float32Array(0);
    const end = p.samples > 0 ? Math.min(pcm.length, p.preSkip + p.samples) : pcm.length;
    return pcm.slice(Math.min(p.preSkip, end), end);
  });
  decodeChain = job.catch(() => {});
  return job;
}

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
const MERGE_GAP = 64 * 1024;
const MAX_REQUEST = 4 * 1024 * 1024;

/** Decoded mono 48 kHz audio for a segment (null if unavailable). */
export function load(seg: Seg): Promise<Float32Array | null> {
  const c = cached(seg.id);
  if (c) return Promise.resolve(c);
  if (seg.status === 'recording' || seg.status === 'raw') return Promise.resolve(null);
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

async function finish(seg: Seg, parsed: Parsed, resolve: (v: Float32Array | null) => void) {
  try {
    const pcm = await decodePackets(parsed);
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
      fetchFile(it.seg.path)
        .then((b) => finish(it.seg, parseOgg(b), it.resolve))
        .catch(() => it.resolve(null));
      continue;
    }
    const l = byPath.get(it.seg.path) ?? [];
    l.push(it);
    byPath.set(it.seg.path, l);
  }
  for (const [path, list] of byPath) {
    list.sort((a, b) => a.seg.offset! - b.seg.offset!);
    // merge neighbouring records into single range requests
    let group: typeof list = [];
    const run = (g: typeof list) => {
      const start = g[0].seg.offset!;
      const end = Math.max(...g.map((x) => x.seg.offset! + x.seg.size));
      fetchRange(path, start, end)
        .then((buf) => {
          for (const it of g) {
            const o = it.seg.offset! - start;
            try {
              finish(it.seg, parseRecord(buf.subarray(o, o + it.seg.size)), it.resolve);
            } catch (e) {
              console.warn('bad record', it.seg.id, e);
              it.resolve(null);
            }
          }
        })
        .catch((e) => {
          console.warn('fetch failed', path, e);
          for (const it of g) it.resolve(null);
        });
    };
    for (const it of list) {
      if (group.length) {
        const gs = group[0].seg.offset!;
        const ge = Math.max(...group.map((x) => x.seg.offset! + x.seg.size));
        const tooFar = it.seg.offset! - ge > MERGE_GAP;
        const tooBig = it.seg.offset! + it.seg.size - gs > MAX_REQUEST;
        if (tooFar || tooBig) {
          run(group);
          group = [];
        }
      }
      group.push(it);
    }
    if (group.length) run(group);
  }
}
