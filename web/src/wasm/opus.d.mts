// Emscripten module built by web/wasm/build.sh (libopus with speech enhancement).
export interface OpusModule {
  HEAPU8: Uint8Array;
  HEAPF32: Float32Array;
  _malloc(n: number): number;
  _free(p: number): void;
  _dec_create(complexity: number): number;
  _dec_reset(d: number): number;
  _dec_set_complexity(d: number, c: number): number;
  _dec_decode(d: number, pkt: number, len: number, out: number): number;
  _dec_destroy(d: number): void;
}
export default function createOpus(opts?: object): Promise<OpusModule>;
