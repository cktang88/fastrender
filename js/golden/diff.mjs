// node golden/diff.mjs a.png b.png [diff.png]: mean absolute error and share of pixels off by > 24/255.
import { readFileSync, writeFileSync } from 'node:fs';
import { PNG } from 'pngjs';
const [a, b, out] = process.argv.slice(2);
const A = PNG.sync.read(readFileSync(a)), B = PNG.sync.read(readFileSync(b));
if (A.width !== B.width || A.height !== B.height) throw new Error(`size mismatch ${A.width}x${A.height} vs ${B.width}x${B.height}`);
const D = new PNG({ width: A.width, height: A.height });
let sum = 0, bad = 0;
const n = A.width * A.height;
for (let i = 0; i < n; i++) {
  let m = 0;
  for (let c = 0; c < 3; c++) {
    const d = Math.abs(A.data[i * 4 + c] - B.data[i * 4 + c]);
    sum += d; m = Math.max(m, d);
  }
  if (m > 24) bad++;
  D.data[i * 4] = Math.min(255, m * 4); D.data[i * 4 + 1] = D.data[i * 4 + 2] = 0; D.data[i * 4 + 3] = 255;
}
if (out) writeFileSync(out, PNG.sync.write(D));
const r = { mae: +(sum / (n * 3)).toFixed(2), badPct: +((bad / n) * 100).toFixed(2) };
console.log(JSON.stringify(r));
