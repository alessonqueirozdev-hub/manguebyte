// Runs the WebAssembly build of the `simd_register_wasm` example: the SIMD
// registers and the lookup tables against their definitions.
//
//   RUSTFLAGS="-C target-feature=+simd128" cargo build --release \
//     -p manguebyte-core --example simd_register_wasm --target wasm32-unknown-unknown
//   node crates/manguebyte-core/examples/simd_register_wasm.mjs \
//     target/wasm32-unknown-unknown/release/examples/simd_register_wasm.wasm
//
// Exits with 1 if any check fails, if the module was built without SIMD or if
// it needs anything from its host.
import fs from "node:fs";

const bytes = fs.readFileSync(process.argv[2]);
const imports = WebAssembly.Module.imports(new WebAssembly.Module(bytes));
console.log("imports:", JSON.stringify(imports));
if (imports.length > 0) process.exit(1);
const { instance } = await WebAssembly.instantiate(bytes, {});
const simd = instance.exports.runs_simd128();
const failed = instance.exports.failed_checks();
console.log(`runs simd128: ${simd}, failed checks: ${failed}`);
process.exit(simd === 1 && failed === 0 ? 0 : 1);
