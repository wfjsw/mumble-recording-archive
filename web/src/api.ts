// Static data layer: everything is read from files the bot publishes
// (catalog.json, index/<day>.json, audio packs via HTTP Range).

export interface Seg {
  id: number;
  speakerId: number;
  channelId: number;
  startMs: number;
  endMs: number;
  /** 0 normal, 1 shout, 2 whisper, 3 heard via channel listener, 31 loopback */
  target: number;
  status: 'recording' | 'raw' | 'ready' | 'error';
  path: string;
  offset: number | null;
  size: number;
  peaks: Uint8Array | null;
}

export interface Speaker {
  id: number;
  name: string;
  userId: number | null;
  firstSeen: number;
  lastSeen: number;
}

export interface Channel {
  id: number;
  mumbleId: number;
  parentId: number | null;
  name: string;
  position: number;
  removedAt: number | null;
}

export interface DayEntry {
  day: string;
  from: number;
  to: number;
  segments: number;
  totalMs: number;
  file: string;
}

export interface Catalog {
  version: number;
  generatedAt: number;
  server: string;
  bot: string;
  bitrate: string;
  peaksPerSec: number;
  stats: {
    segments: number;
    totalMs: number;
    bytes: number;
    rawBytes: number;
    earliestMs: number | null;
    latestMs: number | null;
    pending: number;
  };
  days: DayEntry[];
}

export interface Presence {
  speakerId: number;
  channelId: number;
  startMs: number;
  endMs: number | null;
}

export interface DayIndex {
  day: string;
  from: number;
  to: number;
  generatedAt: number;
  segments: Seg[];
  presence: Presence[];
  uptime: [number, number][];
}

/** Base URL of the bot's public dir; override with ./config.json {"dataUrl": "..."} */
let dataUrl = 'data/';

export function dataHref(rel: string): string {
  return new URL(rel, new URL(dataUrl, location.href)).href;
}

export async function loadConfig() {
  try {
    const res = await fetch('config.json', { cache: 'no-cache' });
    if (res.ok) {
      const c = await res.json();
      if (typeof c.dataUrl === 'string') dataUrl = c.dataUrl.endsWith('/') ? c.dataUrl : `${c.dataUrl}/`;
    }
  } catch {}
}

async function getJson<T>(rel: string): Promise<T> {
  // no-cache: revalidate (cheap 304s) — these files are rewritten in place.
  const res = await fetch(dataHref(rel), { cache: 'no-cache' });
  if (!res.ok) throw new Error(`${rel}: ${res.status} ${res.statusText}`);
  return res.json();
}

export function decodePeaks(b64: string | null): Uint8Array | null {
  if (!b64) return null;
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export const fetchCatalog = () => getJson<Catalog>('catalog.json');

export async function fetchChannels(): Promise<Channel[]> {
  const raw = await getJson<{ channels: any[][] }>('channels.json');
  return raw.channels.map(([id, mumbleId, parentId, name, position, removedAt]) => ({
    id,
    mumbleId,
    parentId,
    name,
    position,
    removedAt,
  }));
}

export async function fetchSpeakers(): Promise<Speaker[]> {
  const raw = await getJson<{ speakers: any[][] }>('speakers.json');
  return raw.speakers.map(([id, name, userId, firstSeen, lastSeen]) => ({ id, name, userId, firstSeen, lastSeen }));
}

export async function fetchDay(file: string): Promise<DayIndex> {
  const raw = await getJson<any>(file);
  const cols: string[] = raw.columns;
  const ix = (n: string) => cols.indexOf(n);
  const [iId, iSp, iCh, iStart, iDur, iTarget, iStatus, iPath, iOff, iSize, iPeaks] = [
    'id', 'speakerId', 'channelId', 'startMs', 'durMs', 'target', 'status', 'path', 'offset', 'size', 'peaks',
  ].map(ix);
  const segments: Seg[] = raw.segments.map((r: any[]) => ({
    id: r[iId],
    speakerId: r[iSp],
    channelId: r[iCh],
    startMs: r[iStart],
    endMs: r[iStart] + r[iDur],
    target: r[iTarget],
    status: r[iStatus],
    path: raw.paths[r[iPath]] ?? '',
    offset: r[iOff],
    size: r[iSize],
    peaks: decodePeaks(r[iPeaks]),
  }));
  return {
    day: raw.day,
    from: raw.from,
    to: raw.to,
    generatedAt: raw.generatedAt,
    segments,
    presence: raw.presence.map((p: any[]) => ({ speakerId: p[0], channelId: p[1], startMs: p[2], endMs: p[3] })),
    uptime: raw.uptime,
  };
}

/** Read bytes [start, end) of a published file. Works with any static server supporting Range. */
export async function fetchRange(rel: string, start: number, end: number): Promise<Uint8Array> {
  const res = await fetch(dataHref(rel), { headers: { Range: `bytes=${start}-${end - 1}` }, cache: 'no-store' });
  if (!res.ok) throw new Error(`${rel}: ${res.status}`);
  const buf = new Uint8Array(await res.arrayBuffer());
  // A server ignoring Range answers 200 with the whole file.
  return res.status === 206 ? buf : buf.subarray(start, end);
}

export async function fetchFile(rel: string): Promise<Uint8Array> {
  const res = await fetch(dataHref(rel), { cache: 'no-store' });
  if (!res.ok) throw new Error(`${rel}: ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}
