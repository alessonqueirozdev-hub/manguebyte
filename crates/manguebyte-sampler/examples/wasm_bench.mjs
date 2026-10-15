// Times the WebAssembly build of the `wasm_bench` example.
//
//   cargo build -p manguebyte-sampler --example wasm_bench --target wasm32-unknown-unknown --release
//   node crates/manguebyte-sampler/examples/wasm_bench.mjs \
//     target/wasm32-unknown-unknown/release/examples/wasm_bench.wasm
//
// For each case it takes the fastest of three runs of two seconds of audio,
// less the fastest of three runs that only build the case, and prints
// nanoseconds per voice and output frame and how many such voices render
// in real time at 48 kHz. Node has no AudioWorklet; this is the engine's
// speed on the main thread.
import fs from "node:fs";
import { performance } from "node:perf_hooks";

const bytes = fs.readFileSync(process.argv[2]);
const { instance } = await WebAssembly.instantiate(bytes, {});
const { bench_cases, bench_case, bench_voice_frames } = instance.exports;
const names = [
  "sinc, own pitch and rate",
  "sinc, 44.1 kHz sample at its pitch",
  "sinc, down a fifth",
  "sinc, up a fifth",
  "sinc, up an octave",
  "sinc, up two octaves",
  "cubic, down a fifth",
  "cubic, up a fifth",
];
const blocks = (2 * 48000) / 128;
const fastest = (f) => {
  let best = Infinity;
  for (let run = 0; run < 3; run++) {
    const start = performance.now();
    f();
    best = Math.min(best, performance.now() - start);
  }
  return best;
};
console.log("case".padEnd(40), "ns/voice/frame".padStart(14), "real-time voices".padStart(16));
let checksum = 0;
for (let index = 0; index < bench_cases(); index++) {
  const setup = fastest(() => bench_case(index, 0));
  const whole = fastest(() => {
    checksum += bench_case(index, blocks);
  });
  const ns = ((whole - setup) * 1e6) / bench_voice_frames(blocks);
  const voices = 1e9 / (ns * 48000);
  console.log(
    (names[index] ?? `case ${index}`).padEnd(40),
    ns.toFixed(1).padStart(14),
    voices.toFixed(0).padStart(16),
  );
}
console.log(`(checksum ${checksum.toFixed(3)})`);
