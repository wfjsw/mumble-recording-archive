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
import { addDays, DAY, startOfDay } from './util';

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

let sticky = { from: 0, to: 0 };

/**
 * Whole days around the view, which decide the lanes: lanes stay put while
 * scrolling within those days. The scope is sticky: zooming in towards a day
 * boundary keeps the days already shown instead of dropping one (which would
 * remove its lanes and shift the rest under the cursor). It is replaced when
 * the view leaves it, or when it is much wider than the view needs.
 */
export const scope = computed(() => {
  const end = ui.view.start + ui.view.width * ui.view.msPerPx;
  const from = startOfDay(ui.view.start);
  const to = addDays(startOfDay(Math.max(from, end - 1)), 1);
  const s = sticky;
  if (from >= s.from && to <= s.to && s.to - s.from <= to - from + 2 * DAY) return s;
  sticky = { from, to };
  return sticky;
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
