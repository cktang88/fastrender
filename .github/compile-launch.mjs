// .github/compile-launch.mjs <label> <bench log> <wall ms> <exit code>
// One compact row (< 300 chars: annotations are cut there) from the `three compile:` line of a `fastrender bench` log.
import { readFileSync } from 'node:fs';
const [label, log, wall, rc] = process.argv.slice(2);
let text = '';
try { text = readFileSync(log, 'utf8'); } catch {}
const line = text.split('\n').filter((l) => l.startsWith('three compile:')).pop() || '';
const g = (re) => ((line.match(re) || [])[1] ?? '?').replace(/[,;]$/, '');
const n = (v) => (v === '?' || v === '-' ? v : String(Math.round(parseFloat(v.replace('+', '')))));
const row = [
  label,
  `wall ${(wall / 1000).toFixed(1)}s`,
  `frame1 ${n(g(/first frame (\S+)/))}`,
  `complete ${n(g(/first complete frame (\S+)/))}`,
  `last-incomplete ${n(g(/last incomplete frame (\S+)/))}`,
  `ready ${n(g(/all pipelines ready (\S+)/))} ms`,
  `waits ${g(/render-thread waits (\d+)/)} (${g(/render-thread waits \d+ \((\d+) ms\)/)} ms)`,
  `skipped ${g(/skipped draws (\d+)/)} draws, ${g(/skipped draws \d+ in (\d+)/)}/${g(/skipped draws \d+ in \d+ of (\d+)/)} frames`,
  `>50ms ${g(/frames over 50 ms: (\d+)/)}, max ${g(/longest (\d+) ms/)}`,
  `libs ${g(/, (\d+) libraries/)}: ${g(/libraries in (\d+) ms/)} ms (${g(/\((\d+) shared\)/)} shared)`,
  `pipes ${g(/, (\d+) pipelines in/)}: ${g(/pipelines in (\d+) ms/)} ms, ${g(/\((\d+) from the binary archive\)/)} archive`,
  `prewarmed ${g(/prewarmed (\d+ \/ \d+)/)}`,
  rc === '0' ? '' : `EXIT ${rc}`,
];
console.log(row.filter(Boolean).join(' | '));
