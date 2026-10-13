// Export rendering off the main thread: fetches and decodes the clips, places
// them sample-accurately, mixes them into the requested outputs and encodes
// WAV / Opus (one multichannel file, or a zip of aligned mono files).

import { Decoder, ENHANCED, fetchRangeUrl, fetchUrl, groupRanges, parse, PLAIN, RATE } from './codec';
import { OggOpusWriter, ZipWriter, toPcm16, wavHeader } from './encode';

export interface ExportItem {
  startMs: number;
  endMs: number;
  /** Output index (channel, or file when split). */
  out: number;
  gain: number;
  href: string;
  cache: RequestCache;
  /** Pack record offset; null = standalone Ogg file. */
  offset: number | null;
  size: number;
}

export interface ExportJob {
  from: number;
  to: number;
  format: 'wav' | 'opus';
  split: boolean;
  labels: string[];
  filename: string;
  /** Decode with speech enhancement (as in playback). */
  enhance: boolean;
  /** Sorted by start. */
  items: ExportItem[];
}

export type ExportMessage = { progress: number } | { blob: Blob; filename: string } | { error: string };

const BLOCK = 10 * RATE;
/** Clips are fetched and decoded this far ahead of the block being mixed. */
const PREFETCH_MS = 30_000;

const post = (m: ExportMessage) => (self as unknown as Worker).postMessage(m);

// ---------- loading ----------

const decoder = new Decoder();
let decodeChain: Promise<unknown> = Promise.resolve();
let complexity = ENHANCED;
const pcm = new Map<ExportItem, Promise<Float32Array | null>>();

function decode(kind: 'record' | 'ogg', bytes: Uint8Array): Promise<Float32Array | null> {
  const job = decodeChain.then(() => decoder.decode(parse(kind, bytes), complexity)).catch(() => null);
  decodeChain = job;
  return job;
}

/** Start loading the given clips (records of one file share Range requests). */
function request(list: ExportItem[]) {
  const byHref = new Map<string, ExportItem[]>();
  for (const it of list) {
    if (pcm.has(it)) continue;
    if (it.offset === null) {
      pcm.set(
        it,
        fetchUrl(it.href, it.cache).then((b) => decode('ogg', b), () => null),
      );
      continue;
    }
    (byHref.get(it.href) ?? byHref.set(it.href, []).get(it.href)!).push(it);
  }
  for (const [href, items] of byHref) {
    const ranges = items.map((it) => ({ offset: it.offset!, size: it.size, it })).sort((a, b) => a.offset - b.offset);
    for (const g of groupRanges(ranges)) {
      const bytes = fetchRangeUrl(href, g.start, g.end, g.items[0].it.cache);
      for (const { offset, size, it } of g.items) {
        pcm.set(
          it,
          bytes.then(
            (b) => decode('record', b.subarray(offset - g.start, offset - g.start + size)),
            () => null,
          ),
        );
      }
    }
  }
}

// ---------- encoders ----------

interface Sink {
  write(chans: Float32Array[]): Promise<void> | void;
  finish(frames: number): Promise<Uint8Array<ArrayBuffer>[]>;
}

function wavSink(channels: number): Sink {
  const parts: Uint8Array<ArrayBuffer>[] = [];
  return {
    write: (c) => void parts.push(toPcm16(c)),
    finish: async (frames) => [wavHeader(channels, frames) as Uint8Array<ArrayBuffer>, ...parts],
  };
}

function opusSink(channels: number): Sink {
  let writer: OggOpusWriter | null = null;
  let failure: Error | null = null;
  let frames = 0;
  const AE = (globalThis as any).AudioEncoder;
  const AD = (globalThis as any).AudioData;
  const enc = new AE({
    output: (chunk: any, meta: any) => {
      if (!writer) {
        // OpusHead from the encoder carries the real pre-skip.
        const desc: ArrayBuffer | undefined = meta?.decoderConfig?.description;
        let preSkip = 312;
        if (desc && desc.byteLength >= 12) preSkip = new DataView(desc).getUint16(10, true);
        writer = new OggOpusWriter(channels, preSkip);
      }
      const data = new Uint8Array(chunk.byteLength);
      chunk.copyTo(data);
      writer.write(data, Math.round(((chunk.duration ?? 20000) * RATE) / 1e6));
    },
    error: (e: Error) => (failure = e),
  });
  // DTX: exported tracks are mostly silence between clips, which then costs next to nothing.
  enc.configure({ codec: 'opus', sampleRate: RATE, numberOfChannels: channels, bitrate: 24000 * channels, opus: { usedtx: true } });
  return {
    write: (chans) => {
      const n = chans[0].length;
      const planar = new Float32Array(n * channels);
      chans.forEach((c, i) => planar.set(c, i * n));
      const ad = new AD({
        format: 'f32-planar',
        sampleRate: RATE,
        numberOfFrames: n,
        numberOfChannels: channels,
        timestamp: Math.round((frames / RATE) * 1e6),
        data: planar,
      });
      frames += n;
      enc.encode(ad);
      ad.close();
    },
    finish: async (total) => {
      await enc.flush();
      enc.close();
      if (failure) throw failure;
      if (!writer) writer = new OggOpusWriter(channels);
      return (writer as OggOpusWriter).finish(total);
    },
  };
}

// ---------- rendering ----------

async function run(job: ExportJob): Promise<{ blob: Blob; filename: string }> {
  complexity = job.enhance ? ENHANCED : PLAIN;
  const total = Math.round(((job.to - job.from) * RATE) / 1000);
  const nOut = job.labels.length;
  const mk = (ch: number) => (job.format === 'opus' ? opusSink(ch) : wavSink(ch));
  const sinks: Sink[] = job.split ? job.labels.map(() => mk(1)) : [mk(nOut)];

  const items = job.items;
  let next = 0;
  let ahead = 0;
  let active: ExportItem[] = [];
  for (let pos = 0; pos < total; pos += BLOCK) {
    const end = Math.min(total, pos + BLOCK);
    const blockFrom = job.from + (pos * 1000) / RATE;
    const blockTo = job.from + (end * 1000) / RATE;
    while (next < items.length && items[next].startMs < blockTo) active.push(items[next++]);
    for (const it of active.filter((s) => s.endMs <= blockFrom)) pcm.delete(it); // done with it
    active = active.filter((s) => s.endMs > blockFrom);
    // Load this block's clips and prefetch the next ones while mixing.
    ahead = Math.max(ahead, next);
    const want = [...active];
    while (ahead < items.length && items[ahead].startMs < blockTo + PREFETCH_MS) want.push(items[ahead++]);
    request(want);

    const chans = Array.from({ length: nOut }, () => new Float32Array(end - pos));
    const pcms = await Promise.all(active.map((s) => pcm.get(s)!));
    active.forEach((s, i) => {
      const p = pcms[i];
      if (!p || !s.gain) return;
      const out = chans[s.out];
      const start = Math.round(((s.startMs - job.from) * RATE) / 1000); // relative to range start
      const a = Math.max(pos, start);
      const b = Math.min(end, start + p.length);
      for (let t = a; t < b; t++) out[t - pos] += p[t - start] * s.gain;
    });
    if (job.split) await Promise.all(sinks.map((k, i) => k.write([chans[i]])));
    else await sinks[0].write(chans);
    post({ progress: end / total });
  }

  if (job.split) {
    const zip = new ZipWriter();
    const ext = job.format === 'opus' ? 'opus' : 'wav';
    const manifest = {
      from: job.from,
      to: job.to,
      start: new Date(job.from).toISOString(),
      sampleRate: RATE,
      tracks: [] as { file: string; label: string }[],
    };
    for (let i = 0; i < nOut; i++) {
      const safe = job.labels[i].replace(/[^\p{L}\p{N}._-]+/gu, '_').slice(0, 60);
      const file = `${String(i + 1).padStart(2, '0')}_${safe}.${ext}`;
      manifest.tracks.push({ file, label: job.labels[i] });
      zip.add(file, await sinks[i].finish(total));
    }
    zip.add('manifest.json', [new TextEncoder().encode(JSON.stringify(manifest, null, 2)) as Uint8Array<ArrayBuffer>]);
    return { blob: new Blob(zip.finish(), { type: 'application/zip' }), filename: job.filename };
  }
  const parts = await sinks[0].finish(total);
  return { blob: new Blob(parts, { type: job.format === 'opus' ? 'audio/ogg' : 'audio/wav' }), filename: job.filename };
}

self.onmessage = (e: MessageEvent<ExportJob>) => {
  run(e.data).then(post, (err) => post({ error: (err as Error).message ?? String(err) }));
};
