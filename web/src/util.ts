export const SEC = 1000;
export const MIN = 60 * SEC;
export const HOUR = 60 * MIN;
export const DAY = 24 * HOUR;

const pad = (n: number, w = 2) => String(n).padStart(w, '0');

export function fmtClock(ms: number, withMs = false): string {
  const d = new Date(ms);
  const s = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  return withMs ? `${s}.${pad(d.getMilliseconds(), 3)}` : s;
}

export function fmtDate(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
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

export function startOfDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

export function addDays(ms: number, n: number): number {
  const d = new Date(ms);
  d.setDate(d.getDate() + n);
  return d.getTime();
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
  const d = new Date(ms);
  if (step >= DAY || (d.getHours() === 0 && d.getMinutes() === 0 && d.getSeconds() === 0 && d.getMilliseconds() === 0)) {
    return `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  }
  if (step < SEC) return `${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3).slice(0, 1)}`;
  if (step < MIN) return fmtClock(ms);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
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
