// Small container writers for client-side exports: WAV, Ogg Opus, zip (stored).

export function wavHeader(channels: number, frames: number, rate = 48000): Uint8Array {
  const data = frames * channels * 2;
  const b = new ArrayBuffer(68);
  const v = new DataView(b);
  const str = (o: number, s: string) => [...s].forEach((c, i) => v.setUint8(o + i, c.charCodeAt(0)));
  str(0, 'RIFF');
  v.setUint32(4, Math.min(0xffffffff, data + 60), true);
  str(8, 'WAVEfmt ');
  v.setUint32(16, 40, true);
  v.setUint16(20, 0xfffe, true); // WAVE_FORMAT_EXTENSIBLE
  v.setUint16(22, channels, true);
  v.setUint32(24, rate, true);
  v.setUint32(28, rate * channels * 2, true);
  v.setUint16(32, channels * 2, true);
  v.setUint16(34, 16, true);
  v.setUint16(36, 22, true);
  v.setUint16(38, 16, true);
  // Channel mask: mono/stereo get speaker positions; more tracks are left unassigned.
  v.setUint32(40, channels === 1 ? 4 : channels === 2 ? 3 : 0, true);
  const guid = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71];
  guid.forEach((x, i) => v.setUint8(44 + i, x));
  str(60, 'data');
  v.setUint32(64, Math.min(0xffffffff, data), true);
  return new Uint8Array(b);
}

/** Interleave planar float channels into 16-bit PCM. */
export function toPcm16(chans: Float32Array[]): Uint8Array<ArrayBuffer> {
  const n = chans[0]?.length ?? 0;
  const out = new Int16Array(n * chans.length);
  for (let i = 0; i < n; i++) {
    for (let c = 0; c < chans.length; c++) {
      const v = Math.max(-1, Math.min(1, chans[c][i]));
      out[i * chans.length + c] = Math.round(v * 32767);
    }
  }
  return new Uint8Array(out.buffer);
}

// ---------- CRCs ----------

const oggTable = (() => {
  const t = new Uint32Array(256);
  for (let i = 0; i < 256; i++) {
    let r = i << 24;
    for (let k = 0; k < 8; k++) r = r & 0x80000000 ? (r << 1) ^ 0x04c11db7 : r << 1;
    t[i] = r >>> 0;
  }
  return t;
})();

function oggCrc(d: Uint8Array): number {
  let c = 0;
  for (const b of d) c = ((c << 8) ^ oggTable[((c >>> 24) ^ b) & 0xff]) >>> 0;
  return c;
}

const zipTable = (() => {
  const t = new Uint32Array(256);
  for (let i = 0; i < 256; i++) {
    let c = i;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[i] = c >>> 0;
  }
  return t;
})();

export function crc32(parts: Uint8Array[]): number {
  let c = 0xffffffff;
  for (const d of parts) for (const b of d) c = zipTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

// ---------- Ogg Opus ----------

export class OggOpusWriter {
  parts: Uint8Array<ArrayBuffer>[] = [];
  private seq = 0;
  private granule: number;
  private pending: { data: Uint8Array; granule: number }[] = [];
  private pendingBytes = 0;

  constructor(
    channels: number,
    private preSkip = 312,
    private serial = (Math.random() * 0xffffffff) >>> 0,
  ) {
    const head = new Uint8Array(19);
    const hv = new DataView(head.buffer);
    'OpusHead'.split('').forEach((c, i) => (head[i] = c.charCodeAt(0)));
    head[8] = 1;
    head[9] = channels;
    hv.setUint16(10, preSkip, true);
    hv.setUint32(12, 48000, true);
    hv.setInt16(16, 0, true);
    head[18] = 0;
    const vendor = 'mumble-archive';
    const tags = new Uint8Array(8 + 4 + vendor.length + 4);
    'OpusTags'.split('').forEach((c, i) => (tags[i] = c.charCodeAt(0)));
    new DataView(tags.buffer).setUint32(8, vendor.length, true);
    vendor.split('').forEach((c, i) => (tags[12 + i] = c.charCodeAt(0)));
    this.page([head], 0, 0x02);
    this.page([tags], 0, 0);
    this.granule = preSkip;
  }

  private page(packets: Uint8Array[], granule: number, flags: number) {
    const lacing: number[] = [];
    for (const p of packets) {
      let n = p.length;
      while (n >= 255) {
        lacing.push(255);
        n -= 255;
      }
      lacing.push(n);
    }
    const body = packets.reduce((a, p) => a + p.length, 0);
    const page = new Uint8Array(27 + lacing.length + body);
    const v = new DataView(page.buffer);
    'OggS'.split('').forEach((c, i) => (page[i] = c.charCodeAt(0)));
    page[5] = flags;
    v.setBigInt64(6, BigInt(granule), true);
    v.setUint32(14, this.serial, true);
    v.setUint32(18, this.seq++, true);
    page[26] = lacing.length;
    page.set(lacing, 27);
    let o = 27 + lacing.length;
    for (const p of packets) {
      page.set(p, o);
      o += p.length;
    }
    v.setUint32(22, oggCrc(page), true);
    this.parts.push(page);
  }

  private flush(flags: number) {
    if (!this.pending.length) return;
    const last = this.pending[this.pending.length - 1].granule;
    this.page(
      this.pending.map((p) => p.data),
      last,
      flags,
    );
    this.pending = [];
    this.pendingBytes = 0;
  }

  write(packet: Uint8Array, samples: number) {
    // keep pages under 255 lacing values
    if (this.pending.length >= 50 || this.pendingBytes > 16000) this.flush(0);
    this.granule += samples;
    this.pending.push({ data: packet, granule: this.granule });
    this.pendingBytes += packet.length;
  }

  finish(totalSamples?: number): Uint8Array<ArrayBuffer>[] {
    if (totalSamples !== undefined && this.pending.length) {
      this.pending[this.pending.length - 1].granule = this.preSkip + totalSamples;
    }
    this.flush(0x04);
    return this.parts;
  }
}

// ---------- zip (stored, small files only: < 4 GB) ----------

export class ZipWriter {
  parts: Uint8Array<ArrayBuffer>[] = [];
  private offset = 0;
  private central: Uint8Array<ArrayBuffer>[] = [];
  private count = 0;

  private push(b: Uint8Array<ArrayBuffer>) {
    this.parts.push(b);
    this.offset += b.length;
  }

  add(name: string, data: Uint8Array<ArrayBuffer>[]) {
    const nameBytes = new TextEncoder().encode(name);
    const size = data.reduce((a, d) => a + d.length, 0);
    const crc = crc32(data);
    const d = new Date();
    const time = (d.getHours() << 11) | (d.getMinutes() << 5) | (d.getSeconds() >> 1);
    const date = ((d.getFullYear() - 1980) << 9) | ((d.getMonth() + 1) << 5) | d.getDate();
    const local = new Uint8Array(30 + nameBytes.length);
    const lv = new DataView(local.buffer);
    lv.setUint32(0, 0x04034b50, true);
    lv.setUint16(4, 20, true);
    lv.setUint16(6, 0x0800, true);
    lv.setUint16(10, time, true);
    lv.setUint16(12, date, true);
    lv.setUint32(14, crc, true);
    lv.setUint32(18, size, true);
    lv.setUint32(22, size, true);
    lv.setUint16(26, nameBytes.length, true);
    local.set(nameBytes, 30);
    const at = this.offset;
    this.push(local);
    for (const p of data) this.push(p);
    const cd = new Uint8Array(46 + nameBytes.length);
    const cv = new DataView(cd.buffer);
    cv.setUint32(0, 0x02014b50, true);
    cv.setUint16(4, 20, true);
    cv.setUint16(6, 20, true);
    cv.setUint16(8, 0x0800, true);
    cv.setUint16(12, time, true);
    cv.setUint16(14, date, true);
    cv.setUint32(16, crc, true);
    cv.setUint32(20, size, true);
    cv.setUint32(24, size, true);
    cv.setUint16(28, nameBytes.length, true);
    cv.setUint32(42, at, true);
    cd.set(nameBytes, 46);
    this.central.push(cd);
    this.count++;
  }

  finish(): Uint8Array<ArrayBuffer>[] {
    const start = this.offset;
    let len = 0;
    for (const c of this.central) {
      this.push(c);
      len += c.length;
    }
    const end = new Uint8Array(22);
    const v = new DataView(end.buffer);
    v.setUint32(0, 0x06054b50, true);
    v.setUint16(8, this.count, true);
    v.setUint16(10, this.count, true);
    v.setUint32(12, len, true);
    v.setUint32(16, start, true);
    this.push(end);
    return this.parts;
  }
}
