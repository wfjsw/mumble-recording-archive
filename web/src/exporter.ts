// Client-side export of a time range: the plan (which clips, which outputs) is
// made here; rendering (decode, mix, encode) runs in a Web Worker.

import { fileUrl, type Seg } from './api';
import type { ExportItem, ExportJob, ExportMessage } from './export.worker';
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
import { fmtStamp, HOUR } from './util';

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
  /** One file per output (zip) instead of one multichannel file. */
  split: boolean;
  segs: Seg[];
  speechMs: number;
  warnings: string[];
  errors: string[];
  filename: string;
}

export const opusSupported = typeof (globalThis as any).AudioEncoder !== 'undefined';

/**
 * Stems are always separate files. Multichannel layouts in Opus are too:
 * Opus beyond stereo is poorly supported by players and editors, so each
 * speaker track (or Mumble channel) becomes its own aligned file.
 */
export const splitsFiles = (o: Pick<ExportOptions, 'layout' | 'format'>) =>
  o.layout === 'stems' || (o.format === 'opus' && (o.layout === 'tracks' || o.layout === 'channels'));

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
  const split = splitsFiles(o);
  if (o.format === 'opus' && !opusSupported) errors.push('This browser has no WebCodecs AudioEncoder; choose WAV.');
  if (o.format === 'wav' && !split && outputs.length > 2) {
    warnings.push('Multichannel WAV: channel N = track N (no speaker positions). Audacity/Audition import it as separate tracks.');
  }
  const speechMs = segs.reduce((a, s) => a + Math.min(s.endMs, o.to) - Math.max(s.startMs, o.from), 0);
  const stamp = fmtStamp(o.from);
  const ext = split ? 'zip' : o.format === 'opus' ? 'opus' : 'wav';
  const name = { downmix: 'downmix', tracks: 'multitrack', channels: 'channels', stems: 'stems' }[o.layout];
  const filename = `mumble_${stamp}_${Math.round((o.to - o.from) / 1000)}s_${name}.${ext}`;
  return { outputs, split, segs, speechMs, warnings, errors, filename };
}

/** Render the export in a Web Worker (fetch, decode, mix, encode); resolves with the file. */
export function runExport(
  o: ExportOptions,
  plan: Plan,
  onProgress: (f: number) => void,
  signal: AbortSignal,
): Promise<{ blob: Blob; filename: string }> {
  const outOf = new Map<string, number>();
  plan.outputs.forEach((out, i) => out.keys.forEach((k) => outOf.set(k, i)));
  const items: ExportItem[] = plan.segs
    .map((s) => {
      const u = fileUrl(s.path, s.offset !== null);
      return {
        startMs: s.startMs,
        endMs: s.endMs,
        out: outOf.get(trackKey(s.channelId, s.speakerId))!,
        gain: o.applyMix ? trackGain(s.channelId, s.speakerId) : 1,
        href: u.href,
        cache: u.cache,
        offset: s.offset,
        size: s.size,
      };
    })
    .sort((a, b) => a.startMs - b.startMs);
  const job: ExportJob = {
    from: o.from,
    to: o.to,
    format: o.format,
    split: plan.split,
    labels: plan.outputs.map((x) => x.label),
    filename: plan.filename,
    items,
  };
  return new Promise((resolve, reject) => {
    const w = new Worker(new URL('./export.worker.ts', import.meta.url), { type: 'module' });
    const stop = () => {
      w.terminate();
      reject(new DOMException('aborted', 'AbortError'));
    };
    signal.addEventListener('abort', stop, { once: true });
    const done = () => {
      signal.removeEventListener('abort', stop);
      w.terminate();
    };
    w.onmessage = (e: MessageEvent<ExportMessage>) => {
      const m = e.data;
      if ('progress' in m) onProgress(m.progress);
      else if ('blob' in m) {
        done();
        resolve(m);
      } else {
        done();
        reject(new Error(m.error));
      }
    };
    w.onerror = (e) => {
      done();
      reject(new Error(e.message || 'export worker failed'));
    };
    w.postMessage(job);
  });
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
