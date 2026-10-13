// Minimal libopus decoder API for the browser: mono 48 kHz float output.
#include <stdlib.h>
#include "opus.h"

OpusDecoder *dec_create(int complexity) {
  int err;
  OpusDecoder *d = opus_decoder_create(48000, 1, &err);
  if (err != OPUS_OK) return NULL;
  opus_decoder_ctl(d, OPUS_SET_COMPLEXITY(complexity));
  return d;
}
int dec_reset(OpusDecoder *d) { return opus_decoder_ctl(d, OPUS_RESET_STATE); }
int dec_set_complexity(OpusDecoder *d, int c) { return opus_decoder_ctl(d, OPUS_SET_COMPLEXITY(c)); }
/* Returns samples decoded (<= 5760) or a negative error. */
int dec_decode(OpusDecoder *d, const unsigned char *pkt, int len, float *out) {
  return opus_decode_float(d, pkt, len, out, 5760, 0);
}
void dec_destroy(OpusDecoder *d) { opus_decoder_destroy(d); }
