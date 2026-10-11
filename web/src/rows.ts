import { computed } from 'vue';
import {
  tree,
  dataVersion,
  isVisible,
  segmentsIn,
  speakerName,
  trackKey,
  ui,
} from './state';
import { DAY, startOfDay } from './util';

export const RULER_H = 30;
export const TRACK_H = 58;
export const CHANNEL_H = 28;
export const COLLAPSED_H = 46;

export interface Row {
  kind: 'channel' | 'track';
  channelId: number;
  speakerId?: number;
  key: string;
  y: number;
  h: number;
  /** Speakers in this channel (for channel rows). */
  speakers: number[];
}

/** Days around the view: lanes stay stable while scrolling within them. */
export const scope = computed(() => {
  const from = startOfDay(ui.view.start);
  const to = startOfDay(ui.view.start + ui.view.width * ui.view.msPerPx) + DAY;
  return { from, to };
});

export const rows = computed<Row[]>(() => {
  void dataVersion.value;
  const { from, to } = scope.value;
  const byChannel = new Map<number, Set<number>>();
  for (const s of segmentsIn(from, to)) {
    (byChannel.get(s.channelId) ?? byChannel.set(s.channelId, new Set()).get(s.channelId)!).add(s.speakerId);
  }
  const out: Row[] = [];
  let y = 0;
  // Active channels in tree order; channels unknown to the tree go last.
  const nodes = tree.value.nodes;
  const order = [...byChannel.keys()].sort((a, b) => (nodes.get(a)?.index ?? 1e9) - (nodes.get(b)?.index ?? 1e9));
  for (const ch of order) {
    if (ui.hiddenChannels.has(ch)) continue;
    const sp = [...(byChannel.get(ch) ?? [])]
      .filter((s) => isVisible(ch, s))
      .sort((a, b) => speakerName(a).localeCompare(speakerName(b)));
    if (!sp.length) continue;
    const collapsed = ui.collapsed.has(ch);
    const h = collapsed ? COLLAPSED_H : CHANNEL_H;
    out.push({ kind: 'channel', channelId: ch, key: `c${ch}`, y, h, speakers: sp });
    y += h;
    if (collapsed) continue;
    for (const s of sp) {
      out.push({ kind: 'track', channelId: ch, speakerId: s, key: trackKey(ch, s), y, h: TRACK_H, speakers: [s] });
      y += TRACK_H;
    }
  }
  return out;
});

export const contentHeight = computed(() => {
  const r = rows.value;
  return r.length ? r[r.length - 1].y + r[r.length - 1].h : 0;
});
