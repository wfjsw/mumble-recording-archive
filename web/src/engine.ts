// Multitrack playback: segments are decoded on demand and scheduled on a
// Web Audio timeline anchored to archive time. Each (channel, speaker) track
// has its own gain node, so mute/solo/volume changes apply instantly.

import * as audio from './audio';
import type { Seg } from './api';
import {
  catalog,
  ensureRange,
  isAudible,
  isVisible,
  nextSegment,
  segmentsIn,
  trackGain,
  trackKey,
  ui,
} from './state';

const LOOKAHEAD_MS = 4000;
const PREFETCH_MS = 20000;
const SKIP_GAP_MS = 1500;

class Engine {
  private ctx: AudioContext | null = null;
  private master: GainNode | null = null;
  private analyser: AnalyserNode | null = null;
  private nodes = new Map<string, GainNode>();
  private sources = new Map<number, AudioBufferSourceNode | null>();
  private anchorCtx = 0;
  private anchorT = 0;
  private gen = 0;
  private timer: ReturnType<typeof setInterval> | undefined;
  private skipping = false;

  private ensureCtx(): AudioContext {
    if (!this.ctx) {
      this.ctx = new AudioContext({ latencyHint: 'playback' });
      this.master = this.ctx.createGain();
      this.analyser = this.ctx.createAnalyser();
      this.analyser.fftSize = 1024;
      this.master.connect(this.analyser);
      this.analyser.connect(this.ctx.destination);
      this.applyGains();
    }
    return this.ctx;
  }

  /** Current archive time (ms) under the playhead. */
  now(): number {
    if (!ui.playing || !this.ctx) return ui.playhead;
    return this.anchorT + Math.max(0, this.ctx.currentTime - this.anchorCtx) * 1000;
  }

  /** Master output level 0..1 for the meter. */
  level(): number {
    if (!this.analyser || !ui.playing) return 0;
    const buf = new Float32Array(this.analyser.fftSize);
    this.analyser.getFloatTimeDomainData(buf);
    let m = 0;
    for (const v of buf) m = Math.max(m, Math.abs(v));
    return m;
  }

  async play(at = ui.playhead) {
    const ctx = this.ensureCtx();
    if (ctx.state === 'suspended') await ctx.resume();
    this.stopSources();
    this.gen++;
    this.anchorCtx = ctx.currentTime + 0.08;
    this.anchorT = at;
    ui.playhead = at;
    ui.playing = true;
    clearInterval(this.timer);
    this.timer = setInterval(() => this.tick(), 100);
    this.tick();
  }

  pause() {
    if (!ui.playing) return;
    ui.playhead = this.now();
    ui.playing = false;
    this.gen++;
    clearInterval(this.timer);
    this.stopSources();
  }

  /** Range playback is confined to, if a selection exists. */
  private range() {
    const s = ui.selection;
    return s && s.to - s.from > 50 ? s : null;
  }

  /**
   * User-initiated play. With a selection: start in place if the playhead is
   * inside it, otherwise at its start; playback ends at its end.
   */
  start() {
    const r = this.range();
    const t = ui.playhead;
    this.play(r && (t < r.from || t >= r.to) ? r.from : t);
  }

  toggle() {
    if (ui.playing) this.pause();
    else this.start();
  }

  seek(t: number) {
    ui.playhead = t;
    if (ui.playing) this.play(t);
  }

  /** Re-apply mixer state (mute/solo/gain/filters) to the graph. */
  applyGains() {
    if (!this.ctx || !this.master) return;
    const now = this.ctx.currentTime;
    this.master.gain.setTargetAtTime(ui.mix.master, now, 0.015);
    for (const [key, node] of this.nodes) {
      const [c, s] = key.split(':').map(Number);
      node.gain.setTargetAtTime(trackGain(c, s), now, 0.015);
    }
    // Tracks hidden by filters are not scheduled at all; drop playing ones.
    for (const [id, src] of this.sources) {
      const seg = (src as any)?.__seg as Seg | undefined;
      if (seg && !isVisible(seg.channelId, seg.speakerId)) {
        try {
          src!.stop();
        } catch {}
        this.sources.delete(id);
      }
    }
  }

  private node(key: string): GainNode {
    let n = this.nodes.get(key);
    if (!n) {
      n = this.ctx!.createGain();
      const [c, s] = key.split(':').map(Number);
      n.gain.value = trackGain(c, s);
      n.connect(this.master!);
      this.nodes.set(key, n);
    }
    return n;
  }

  private stopSources() {
    for (const src of this.sources.values()) {
      try {
        src?.stop();
      } catch {}
    }
    this.sources.clear();
  }

  private tick() {
    if (!ui.playing) return;
    const t = this.now();
    ui.playhead = t;

    const r = this.range();
    if (r && t >= r.to) {
      if (ui.loop) this.play(r.from);
      else {
        this.pause();
        ui.playhead = r.to;
      }
      return;
    }

    void ensureRange(t - 1000, t + PREFETCH_MS);
    const visible = (s: Seg) => isVisible(s.channelId, s.speakerId);
    for (const s of segmentsIn(t, t + LOOKAHEAD_MS)) {
      if (visible(s) && !this.sources.has(s.id)) this.schedule(s, this.gen);
    }
    for (const s of segmentsIn(t + LOOKAHEAD_MS, t + PREFETCH_MS)) if (visible(s)) void audio.load(s);

    if (ui.skipSilence && !this.skipping) {
      const soon = segmentsIn(t, t + SKIP_GAP_MS).some(isAudible);
      if (!soon) {
        this.skipping = true;
        const gen = this.gen;
        nextSegment(t, isAudible)
          .then((next) => {
            if (gen !== this.gen || !ui.playing) return;
            const r = this.range();
            // Inside a selection, never skip past its end: the end-of-range check takes over.
            const target = next && r ? Math.min(next.startMs, r.to) : next?.startMs;
            if (target !== undefined && target - this.now() > SKIP_GAP_MS) this.play(r && target === r.to ? r.to : target - 300);
            else if (!next && this.now() > (catalog.value?.stats.latestMs ?? 0) + 2000) this.pause();
          })
          .finally(() => (this.skipping = false));
      }
    }
  }

  private async schedule(s: Seg, gen: number) {
    this.sources.set(s.id, null); // placeholder: prevents double scheduling
    const pcm = await audio.load(s);
    if (gen !== this.gen || !ui.playing || !pcm || !pcm.length) {
      if (gen === this.gen && !pcm) this.sources.delete(s.id);
      return;
    }
    const ctx = this.ctx!;
    const buf = ctx.createBuffer(1, pcm.length, audio.RATE);
    buf.copyToChannel(pcm as Float32Array<ArrayBuffer>, 0);
    let when = this.anchorCtx + (s.startMs - this.anchorT) / 1000;
    let offset = 0;
    const earliest = ctx.currentTime + 0.02;
    if (when < earliest) {
      offset = earliest - when;
      when = earliest;
    }
    if (offset >= buf.duration) {
      return; // already past (keep placeholder so it is not retried)
    }
    const src = ctx.createBufferSource();
    src.buffer = buf;
    (src as any).__seg = s;
    src.connect(this.node(trackKey(s.channelId, s.speakerId)));
    src.start(when, offset);
    src.onended = () => {
      if (this.sources.get(s.id) === src) this.sources.delete(s.id);
    };
    this.sources.set(s.id, src);
  }
}

export const engine = new Engine();
