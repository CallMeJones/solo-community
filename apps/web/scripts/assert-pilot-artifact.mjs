import { readdir, readFile, stat } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';

const root = path.resolve(process.cwd(), 'dist');
const forbidden = [
  { label: 'deterministic graph fixture', pattern: /mock graph/i },
  { label: 'graph fixture episode content', pattern: /Met Alice for coffee at the new place/i },
  { label: 'graph fixture review content', pattern: /Reviewed PR #142 with Bob/i },
];

async function walk(directory) {
  const files = [];
  for (const entry of await readdir(directory)) {
    const target = path.join(directory, entry);
    const info = await stat(target);
    if (info.isDirectory()) files.push(...(await walk(target)));
    else files.push(target);
  }
  return files;
}

const files = await walk(root);
const emitted = files.filter((file) => /\.(?:js|mjs|cjs|json|html)$/i.test(file));
const failures = [];
for (const file of emitted) {
  const relative = path.relative(root, file).replaceAll('\\', '/');
  for (const rule of forbidden) {
    if (rule.pattern.test(relative)) failures.push(`${rule.label} in emitted filename ${relative}`);
  }
  const contents = await readFile(file, 'utf8');
  for (const rule of forbidden) {
    if (rule.pattern.test(contents)) failures.push(`${rule.label} in ${relative}`);
  }
}

if (failures.length > 0) {
  process.stderr.write(
    `Pilot artifact includes forbidden fixture code:\n${failures.join('\n')}\n`,
  );
  process.exit(1);
}

process.stdout.write(`Pilot artifact boundary verified across ${emitted.length} emitted files.\n`);
