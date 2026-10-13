#!/bin/sh
# Builds web/src/wasm/opus.{mjs,wasm}: libopus (the copy bundled with the Rust
# opusic-sys crate, so bot and browser use the same version) with Opus' speech
# enhancement (OSCE: LACE/NoLACE) for decoding in the browser.
#
#   docker run --rm -v "$PWD:/src" -w /src emscripten/emsdk:latest sh web/wasm/build.sh
#
# -msse4.1 lets libopus' neural-network code use its SSE intrinsics, which
# Emscripten maps to WebAssembly SIMD (about 4.7x faster NoLACE than the
# generic C path). DRED and the WB->FB bandwidth extension are left out
# (not useful for stored audio; BWE alone would add ~0.6 MB).
set -e
OPUS=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/opusic-sys-0.7.*/opus 2>/dev/null | tail -1)
OPUS=${OPUS_SRC:-$OPUS}
[ -d "$OPUS" ] || { echo "set OPUS_SRC to the opusic-sys opus directory"; exit 1; }
W=$(mktemp -d)
cp -r "$OPUS" "$W/opus"
sed -i 's/ENABLE_OSCE ENABLE_OSCE_BWE)/ENABLE_OSCE)/' "$W/opus/CMakeLists.txt"
emcmake cmake -S "$W/opus" -B "$W/build" -DCMAKE_BUILD_TYPE=Release -DOPUS_OSCE=ON -DOPUS_DRED=OFF \
  -DOPUS_BUILD_PROGRAMS=OFF -DOPUS_BUILD_TESTING=OFF -DOPUS_STACK_PROTECTOR=OFF -DOPUS_FORTIFY_SOURCE=OFF \
  -DCMAKE_C_FLAGS="-O3 -msimd128 -msse4.1" >/dev/null
cmake --build "$W/build" -j8 >/dev/null
emcc -O3 -msimd128 -msse4.1 web/wasm/opus_wrapper.c "$W/build/libopus.a" -I"$W/opus/include" -o web/src/wasm/opus.mjs \
  -sMODULARIZE -sEXPORT_ES6 -sENVIRONMENT=web,worker -sALLOW_MEMORY_GROWTH -sINITIAL_MEMORY=16MB -sFILESYSTEM=0 \
  -sEXPORTED_FUNCTIONS=_dec_create,_dec_reset,_dec_set_complexity,_dec_decode,_dec_destroy,_malloc,_free \
  -sEXPORTED_RUNTIME_METHODS=HEAPU8,HEAPF32
rm -rf "$W"
ls -l web/src/wasm/opus.mjs web/src/wasm/opus.wasm
