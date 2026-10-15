// Runs the WebAssembly build of the `wasm_smoke` example and checks the
// pitch of every note it plays.
//
//   cargo build -p manguebyte-sampler --example wasm_smoke --target wasm32-unknown-unknown
//   node crates/manguebyte-sampler/examples/wasm_smoke.mjs \
//     target/wasm32-unknown-unknown/debug/examples/wasm_smoke.wasm
//
// Exits with 1 if any note is a cent or more out, if the module needs
// anything from its host, or if a WAV file with a hostile chunk size does
// not load (in a debug build an arithmetic overflow traps, which throws).
import fs from "node:fs";

const path = process.argv[2];
const bytes = fs.readFileSync(path);
const { instance } = await WebAssembly.instantiate(bytes, {});
const imports = WebAssembly.Module.imports(new WebAssembly.Module(bytes));
console.log("imports:", JSON.stringify(imports));
if (imports.length > 0) process.exit(1);
let worst = 0;
for (const key of [45, 57, 60, 69, 76, 81, 93]) {
  const measured = instance.exports.smoke_frequency(key);
  const expected = 440 * Math.pow(2, (key - 69) / 12);
  const cents = 1200 * Math.log2(measured / expected);
  worst = Math.max(worst, Math.abs(cents));
  console.log(`key ${key}: ${measured.toFixed(4)} Hz, expected ${expected.toFixed(4)}, ${cents.toFixed(5)} cents`);
}
console.log(`worst ${worst.toFixed(5)} cents`);
const hostile = instance.exports.smoke_hostile_wav();
console.log(`hostile chunk size: ${hostile} bytes of audio loaded`);
process.exit(worst < 1 && hostile === 16 ? 0 : 1);
