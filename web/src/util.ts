import { ref } from 'vue';

export const SEC = 1000;
export const MIN = 60 * SEC;
export const HOUR = 60 * MIN;
export const DAY = 24 * HOUR;

const pad = (n: number, w = 2) => String(n).padStart(w, '0');

// ---------- display time zone ----------
//
// All dates and clocks in the UI are shown in one selectable IANA time zone
// ('' = the browser's own). Wall-clock values are computed as "UTC + offset"
// and read with getUTC*; offsets are cached per 15 minutes (zone transitions
// happen on quarter hours).

const ZONE_KEY = 'mumble-archive.timeZone';

/** Selected zone ('' = browser local). Reactive: views re-render when it changes. */
export const timeZone = ref(localStorage.getItem(ZONE_KEY) ?? '');

export function setTimeZone(tz: string) {
  offsets.clear();
  formatter = null;
  timeZone.value = tz;
  localStorage.setItem(ZONE_KEY, tz);
}

/** The browser's zone name, e.g. "America/Chicago". */
export const browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone;

/** Zones offered in the selector. */
export function zoneNames(): string[] {
  const all: string[] = (Intl as any).supportedValuesOf?.('timeZone') ?? [];
  return all.length ? all : ['UTC', browserZone];
}

let formatter: Intl.DateTimeFormat | null = null;
const offsets = new Map<number, number>();
const BUCKET = 15 * 60_000;

/** Offset (ms) of the display zone from UTC at instant `ms`. */
function offsetAt(ms: number): number {
  const key = Math.floor(ms / BUCKET);
  let o = offsets.get(key);
  if (o === undefined) {
    formatter ??= new Intl.DateTimeFormat('en-US', {
      timeZone: timeZone.value || undefined,
      hourCycle: 'h23',
      year: 'numeric',
      month: 'numeric',
      day: 'numeric',
      hour: 'numeric',
      minute: 'numeric',
      second: 'numeric',
    });
    const t = key * BUCKET;
    const p: Record<string, number> = {};
    for (const x of formatter.formatToParts(new Date(t))) if (x.type !== 'literal') p[x.type] = Number(x.value);
    o = Date.UTC(p.year, p.month - 1, p.day, p.hour, p.minute, p.second) - t;
    if (offsets.size > 10_000) offsets.clear();
    offsets.set(key, o);
  }
  return o;
}

/** Wall-clock time in the display zone as a Date to be read with getUTC*. */
function wall(ms: number): Date {
  void timeZone.value; // reactive dependency
  return new Date(ms + offsetAt(ms));
}

/** Instant at which the display zone's wall clock shows `w` (a "wall as UTC" value). */
function fromWall(w: number): number {
  const t = w - offsetAt(w);
  return w - offsetAt(t);
}

/** Instant of a wall-clock date/time in the display zone (month 1-12). */
export function zonedTime(y: number, month: number, d: number, h = 0, mi = 0): number {
  void timeZone.value;
  return fromWall(Date.UTC(y, month - 1, d, h, mi));
}

export function fmtClock(ms: number, withMs = false): string {
  const d = wall(ms);
  const s = `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}`;
  return withMs ? `${s}.${pad(d.getUTCMilliseconds(), 3)}` : s;
}

export function fmtDate(ms: number): string {
  const d = wall(ms);
  return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}`;
}

/** Compact timestamp for file names, e.g. 20261012-001500. */
export function fmtStamp(ms: number): string {
  const d = wall(ms);
  return `${d.getUTCFullYear()}${pad(d.getUTCMonth() + 1)}${pad(d.getUTCDate())}-${pad(d.getUTCHours())}${pad(d.getUTCMinutes())}${pad(d.getUTCSeconds())}`;
}

export function fmtDateTime(ms: number, withMs = false): string {
  return `${fmtDate(ms)} ${fmtClock(ms, withMs)}`;
}

export function fmtDur(ms: number): string {
  ms = Math.max(0, Math.round(ms));
  const h = Math.floor(ms / HOUR);
  const m = Math.floor((ms % HOUR) / MIN);
  const s = Math.floor((ms % MIN) / SEC);
  const frac = Math.floor((ms % SEC) / 100);
  if (h) return `${h}h ${pad(m)}m ${pad(s)}s`;
  if (m) return `${m}m ${pad(s)}.${frac}s`;
  return `${s}.${frac}s`;
}

export function fmtBytes(n: number): string {
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}

/** Midnight (display zone) of the day containing `ms`. */
export function startOfDay(ms: number): number {
  const w = wall(ms).getTime();
  return fromWall(Math.floor(w / DAY) * DAY);
}

/** Same wall-clock time `n` days later (display zone; days may be 23-25 h). */
export function addDays(ms: number, n: number): number {
  return fromWall(wall(ms).getTime() + n * DAY);
}

/** Golden-angle hue spread so neighbouring ids get distinct colors. */
export function speakerColor(id: number, alpha = 1): string {
  const h = (id * 137.508 + 200) % 360;
  return `hsla(${h.toFixed(0)}, 70%, 62%, ${alpha})`;
}

export function channelColor(id: number, alpha = 1): string {
  const h = (id * 97.3 + 30) % 360;
  return `hsla(${h.toFixed(0)}, 45%, 55%, ${alpha})`;
}

/** Nice ruler step (ms) so labels are at least `minPx` apart. */
export function tickStep(msPerPx: number, minPx = 90): number {
  const steps = [
    100, 200, 500, SEC, 2 * SEC, 5 * SEC, 10 * SEC, 15 * SEC, 30 * SEC, MIN, 2 * MIN, 5 * MIN, 10 * MIN, 15 * MIN,
    30 * MIN, HOUR, 2 * HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY,
  ];
  for (const s of steps) if (s / msPerPx >= minPx) return s;
  return DAY;
}

export function fmtTick(ms: number, step: number): string {
  const d = wall(ms);
  if (step >= DAY || (d.getUTCHours() === 0 && d.getUTCMinutes() === 0 && d.getUTCSeconds() === 0 && d.getUTCMilliseconds() === 0)) {
    return `${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}`;
  }
  if (step < SEC) return `${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}.${pad(d.getUTCMilliseconds(), 3).slice(0, 1)}`;
  if (step < MIN) return fmtClock(ms);
  return `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`;
}

export function clamp(v: number, a: number, b: number) {
  return Math.min(b, Math.max(a, v));
}

export function debounce<T extends (...a: any[]) => void>(fn: T, ms: number): T {
  let t: ReturnType<typeof setTimeout> | undefined;
  return ((...a: any[]) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...a), ms);
  }) as T;
}
