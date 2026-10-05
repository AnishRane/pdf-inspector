// Panic containment check for the native binding.
//
// Build a probe binding first (the feature never ships in releases):
//   npx -p @napi-rs/cli@3 napi build --platform --features panic-probe -o target/panic-probe
// then run: node test-panic.mjs target/panic-probe
//
// A Rust panic must not write document text to stderr (container and
// journald logs), and the JS caller must get a fixed error.
import { spawnSync } from 'child_process';
import { readdirSync } from 'fs';
import { join, resolve } from 'path';
import { strict as assert } from 'assert';

const dir = resolve(process.argv[2] ?? 'target/panic-probe');
const binding = readdirSync(dir).find((f) => f.endsWith('.node'));
assert.ok(binding, `no .node file in ${dir}`);

const secret = 'Celeste Halvorsen-Pryce 987-65-4321 é';
const child = spawnSync(
  process.execPath,
  [
    '-e',
    `const b = require(${JSON.stringify(join(dir, binding))});
     try { b.panicProbe(${JSON.stringify(secret)}); console.log('NO_ERROR'); }
     catch (e) { console.log('ERROR:' + e.message); }`,
  ],
  { encoding: 'utf8' },
);

assert.equal(child.status, 0, `process died: ${child.stderr}`);
const message = child.stdout.trim();
assert.ok(message.startsWith('ERROR:'), `expected a JS error, got ${message}`);
assert.equal(message, 'ERROR:panic_probe: internal error');
for (const fragment of ['Celeste', '987-65-4321', 'byte index', 'lib.rs']) {
  assert.ok(!child.stderr.includes(fragment), `stderr leaked ${fragment}: ${child.stderr}`);
  assert.ok(!message.includes(fragment), `error leaked ${fragment}`);
}
console.log('panic containment: OK');
