// Opus decoding off the main thread: receives a pack record or Ogg file,
// returns mono 48 kHz PCM (transferred, not copied).

import { Decoder, parse } from './codec';

const dec = new Decoder();
let chain: Promise<void> = Promise.resolve();

self.onmessage = (e: MessageEvent<{ id: number; kind: 'record' | 'ogg'; bytes: Uint8Array }>) => {
  const { id, kind, bytes } = e.data;
  // One decoder per worker, used for one stream at a time.
  chain = chain.then(async () => {
    try {
      const pcm = await dec.decode(parse(kind, bytes));
      (self as unknown as Worker).postMessage({ id, pcm }, [pcm.buffer]);
    } catch (err) {
      (self as unknown as Worker).postMessage({ id, error: String(err) });
    }
  });
};
