// Worker-safe audio helpers shared by the main thread and the Web Workers:
// pack record / Ogg parsing, Opus decoding, and grouping of byte ranges into
// few HTTP Range requests. No DOM or app state in here.

import createOpus, { type OpusModule } from './wasm/opus.mjs';

export const RATE = 48000;

export interface Parsed {
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

let opusModule: Promise<OpusModule> | null = null;

/** libopus (WebAssembly, web/wasm/build.sh), loaded once per thread. */
const loadOpus = () => (opusModule ??= createOpus());

/**
 * Decoder complexity: 7 turns on NoLACE, Opus' neural speech enhancement for
 * wideband SILK (our low-bitrate archive); 0 is the plain decoder.
 */
export const ENHANCED = 7;
export const PLAIN = 0;

const MAX_FRAME = 5760; // 120 ms @ 48 kHz
const MAX_PACKET = 1500;

/** A mono 48 kHz Opus decoder for independent streams (state is reset per stream). */
export class Decoder {
  private m: OpusModule | null = null;
  private dec = 0;
  private inp = 0;
  private out = 0;
  private complexity = -1;
  readonly ready: Promise<void>;

  constructor() {
    this.ready = loadOpus().then((m) => {
      this.m = m;
      this.dec = m._dec_create(ENHANCED);
      this.complexity = ENHANCED;
      this.inp = m._malloc(MAX_PACKET);
      this.out = m._malloc(MAX_FRAME * 4);
      if (!this.dec) throw new Error('cannot create Opus decoder');
    });
  }

  async decode(p: Parsed, complexity = ENHANCED): Promise<Float32Array> {
    await this.ready;
    const m = this.m!;
    if (complexity !== this.complexity) {
      m._dec_set_complexity(this.dec, complexity);
      this.complexity = complexity;
    }
    m._dec_reset(this.dec);
    const total = p.packets.length * MAX_FRAME;
    const pcm = new Float32Array(Math.max(total, 0));
    let n = 0;
    for (const pkt of p.packets) {
      if (!pkt.length || pkt.length > MAX_PACKET) continue;
      m.HEAPU8.set(pkt, this.inp);
      const got = m._dec_decode(this.dec, this.inp, pkt.length, this.out);
      if (got <= 0) continue; // corrupt packet: skip (rare; shortens the clip slightly)
      // HEAPF32 may be replaced when memory grows: read it after the call.
      pcm.set(m.HEAPF32.subarray(this.out >> 2, (this.out >> 2) + got), n);
      n += got;
    }
    const end = p.samples > 0 ? Math.min(n, p.preSkip + p.samples) : n;
    return pcm.slice(Math.min(p.preSkip, end), end);
  }
}

/** Decode a pack record or a standalone Ogg file. */
export function parse(kind: 'record' | 'ogg', bytes: Uint8Array): Parsed {
  return kind === 'record' ? parseRecord(bytes) : parseOgg(bytes);
}

export interface RangeItem {
  offset: number;
  size: number;
}

const MERGE_GAP = 64 * 1024;
const MAX_REQUEST = 4 * 1024 * 1024;

/** Group records of one file (sorted by offset) into few Range requests. */
export function groupRanges<T extends RangeItem>(sorted: T[]): { start: number; end: number; items: T[] }[] {
  const out: { start: number; end: number; items: T[] }[] = [];
  for (const it of sorted) {
    const g = out[out.length - 1];
    if (g && it.offset - g.end <= MERGE_GAP && it.offset + it.size - g.start <= MAX_REQUEST) {
      g.items.push(it);
      g.end = Math.max(g.end, it.offset + it.size);
    } else out.push({ start: it.offset, end: it.offset + it.size, items: [it] });
  }
  return out;
}

/** Read bytes [start, end) of a URL with a Range request. */
export async function fetchRangeUrl(href: string, start: number, end: number, cache: RequestCache): Promise<Uint8Array> {
  const res = await fetch(href, { headers: { Range: `bytes=${start}-${end - 1}` }, cache });
  if (!res.ok) throw new Error(`${href}: ${res.status}`);
  const buf = new Uint8Array(await res.arrayBuffer());
  // A server ignoring Range answers 200 with the whole file.
  return res.status === 206 ? buf : buf.subarray(start, end);
}

export async function fetchUrl(href: string, cache: RequestCache): Promise<Uint8Array> {
  const res = await fetch(href, { cache });
  if (!res.ok) throw new Error(`${href}: ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}
