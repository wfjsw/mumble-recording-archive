// Client-side export of a time range: segments are decoded in the browser,
// placed sample-accurately and mixed into the requested channel layout.

import type { Seg } from './api';
import * as audio from './audio';
import { OggOpusWriter, ZipWriter, toPcm16, wavHeader } from './encode';
import {
  channelPath,
  tree,
  ensureRange,
  isVisible,
  segmentsIn,
  speakerName,
  trackGain,
  trackKey,
} from './state';
import { HOUR } from './util';

export type Layout = 'downmix' | 'tracks' | 'channels' | 'stems';
export type Format = 'wav' | 'opus';
export type Scope = 'audible' | 'visible' | 'all';

export interface ExportOptions {
  from: number;
  to: number;
  layout: Layout;
  format: Format;
  scope: Scope;
  /** Apply the mixer's track/channel volumes (otherwise unity gain). */
  applyMix: boolean;
}

export interface Output {
  label: string;
  keys: string[];
}

export interface Plan {
  outputs: Output[];
  segs: Seg[];
  speechMs: number;
  warnings: string[];
  errors: string[];
  filename: string;
}

const RATE = audio.RATE;
const BLOCK = 10 * RATE;

export const opusSupported = typeof (globalThis as any).AudioEncoder !== 'undefined';

const pad = (n: number) => String(n).padStart(2, '0');

export async function makePlan(o: ExportOptions): Promise<Plan> {
  await ensureRange(o.from, o.to);
  const segs = segmentsIn(o.from, o.to).filter((s) => {
    if (s.status !== 'ready' && s.status !== 'error') return false;
    if (o.scope === 'visible') return isVisible(s.channelId, s.speakerId);
    if (o.scope === 'audible') return trackGain(s.channelId, s.speakerId) > 0;
    return true;
  });
  const order = new Map([...tree.value.nodes].map(([id, n]) => [id, n.index]));
  const keys = [...new Set(segs.map((s) => trackKey(s.channelId, s.speakerId)))].sort((a, b) => {
    const [ca, sa] = a.split(':').map(Number);
    const [cb, sb] = b.split(':').map(Number);
    return (order.get(ca) ?? 1e9) - (order.get(cb) ?? 1e9) || speakerName(sa).localeCompare(speakerName(sb));
  });
  const label = (k: string) => {
    const [c, s] = k.split(':').map(Number);
    return `${speakerName(s)} @ ${channelPath(c)}`;
  };
  let outputs: Output[];
  if (o.layout === 'downmix') outputs = keys.length ? [{ label: 'Mix', keys }] : [];
  else if (o.layout === 'channels') {
    const by = new Map<number, string[]>();
    for (const k of keys) {
      const c = Number(k.split(':')[0]);
      (by.get(c) ?? by.set(c, []).get(c)!).push(k);
    }
    outputs = [...by].map(([c, ks]) => ({ label: channelPath(c), keys: ks }));
  } else outputs = keys.map((k) => ({ label: label(k), keys: [k] }));

  const warnings: string[] = [];
  const errors: string[] = [];
  if (!outputs.length) errors.push('No recordings in the selected range for the chosen tracks.');
  if (o.to - o.from > 3 * HOUR) warnings.push('Long range: the export is assembled in memory and may take a while.');
  if (o.format === 'opus') {
    if (!opusSupported) errors.push('This browser has no WebCodecs AudioEncoder; choose WAV.');
    if (o.layout !== 'stems' && outputs.length > 2) errors.push('Opus export supports at most 2 channels here; use WAV for multitrack.');
  }
  if (o.format === 'wav' && o.layout !== 'stems' && outputs.length > 2) {
    warnings.push('Multichannel WAV: channel N = track N (no speaker positions). Audacity/Audition import it as separate tracks.');
  }
  const speechMs = segs.reduce((a, s) => a + Math.min(s.endMs, o.to) - Math.max(s.startMs, o.from), 0);
  const d = new Date(o.from);
  const stamp = `${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}`;
  const ext = o.layout === 'stems' ? 'zip' : o.format === 'opus' ? 'opus' : 'wav';
  const name = { downmix: 'downmix', tracks: 'multitrack', channels: 'channels', stems: 'stems' }[o.layout];
  const filename = `mumble_${stamp}_${Math.round((o.to - o.from) / 1000)}s_${name}.${ext}`;
  return { outputs, segs, speechMs, warnings, errors, filename };
}

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
  const pending: { data: Uint8Array; samples: number }[] = [];
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
      pending.push({ data, samples: Math.round(((chunk.duration ?? 20000) * RATE) / 1e6) });
      for (const p of pending.splice(0)) writer.write(p.data, p.samples);
    },
    error: (e: Error) => (failure = e),
  });
  enc.configure({ codec: 'opus', sampleRate: RATE, numberOfChannels: channels, bitrate: 24000 * channels });
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

export async function runExport(
  o: ExportOptions,
  plan: Plan,
  onProgress: (f: number) => void,
  signal: AbortSignal,
): Promise<{ blob: Blob; filename: string }> {
  const total = Math.round(((o.to - o.from) * RATE) / 1000);
  const nOut = plan.outputs.length;
  const outOf = new Map<string, number>();
  plan.outputs.forEach((out, i) => out.keys.forEach((k) => outOf.set(k, i)));
  const gain = (s: Seg) => (o.applyMix ? trackGain(s.channelId, s.speakerId) : 1);
  const mk = (ch: number) => (o.format === 'opus' ? opusSink(ch) : wavSink(ch));
  const sinks: Sink[] = o.layout === 'stems' ? plan.outputs.map(() => mk(1)) : [mk(nOut)];

  const segs = plan.segs;
  let next = 0;
  let active: Seg[] = [];
  for (let pos = 0; pos < total; pos += BLOCK) {
    if (signal.aborted) throw new DOMException('aborted', 'AbortError');
    const end = Math.min(total, pos + BLOCK);
    const blockFrom = o.from + (pos * 1000) / RATE;
    const blockTo = o.from + (end * 1000) / RATE;
    while (next < segs.length && segs[next].startMs < blockTo) active.push(segs[next++]);
    active = active.filter((s) => s.endMs > blockFrom);
    // prefetch the next block's segments while mixing this one
    for (let i = next; i < segs.length && segs[i].startMs < blockTo + 20000; i++) void audio.load(segs[i]);

    const chans = Array.from({ length: nOut }, () => new Float32Array(end - pos));
    const pcms = await Promise.all(active.map((s) => audio.load(s)));
    active.forEach((s, i) => {
      const pcm = pcms[i];
      if (!pcm) return;
      const g = gain(s);
      if (!g) return;
      const out = chans[outOf.get(trackKey(s.channelId, s.speakerId))!];
      const start = Math.round(((s.startMs - o.from) * RATE) / 1000); // relative to range start
      const a = Math.max(pos, start);
      const b = Math.min(end, start + pcm.length);
      for (let t = a; t < b; t++) out[t - pos] += pcm[t - start] * g;
    });
    if (o.layout === 'stems') await Promise.all(sinks.map((k, i) => k.write([chans[i]])));
    else await sinks[0].write(chans);
    onProgress(end / total);
  }

  if (o.layout === 'stems') {
    const zip = new ZipWriter();
    const ext = o.format === 'opus' ? 'opus' : 'wav';
    const manifest = {
      from: o.from,
      to: o.to,
      start: new Date(o.from).toISOString(),
      sampleRate: RATE,
      tracks: [] as { file: string; label: string }[],
    };
    for (let i = 0; i < nOut; i++) {
      const safe = plan.outputs[i].label.replace(/[^\p{L}\p{N}._-]+/gu, '_').slice(0, 60);
      const file = `${String(i + 1).padStart(2, '0')}_${safe}.${ext}`;
      manifest.tracks.push({ file, label: plan.outputs[i].label });
      zip.add(file, await sinks[i].finish(total));
    }
    zip.add('manifest.json', [new TextEncoder().encode(JSON.stringify(manifest, null, 2)) as Uint8Array<ArrayBuffer>]);
    return { blob: new Blob(zip.finish(), { type: 'application/zip' }), filename: plan.filename };
  }
  const parts = await sinks[0].finish(total);
  const type = o.format === 'opus' ? 'audio/ogg' : 'audio/wav';
  return { blob: new Blob(parts, { type }), filename: plan.filename };
}

export function download(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}
