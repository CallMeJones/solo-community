/**
 * `npm audit`, with a reviewed list of advisories this build accepts.
 *
 * The plain audit is all-or-nothing: an advisory with no fixed version and no
 * reach into the shipped bundle either blocks every release or has to be
 * waved away by dropping the audit level, which would hide the next real one
 * too. So the exceptions are written down in audit-exceptions.json with a
 * reason and the condition that ends them, and anything not on that list
 * fails as before.
 *
 * An exception that is no longer reported fails too. A list that outlives the
 * problem it describes is how a gate stops meaning anything.
 */

import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const exceptionsPath = resolve(here, '..', 'audit-exceptions.json');
const accepted = JSON.parse(readFileSync(exceptionsPath, 'utf8')).accepted ?? [];

function runAudit() {
  try {
    // A fixed command string: `npm` is a shell script on Windows, which
    // execFileSync cannot launch, and there is nothing interpolated here.
    return execSync('npm audit --json --audit-level=low', {
      cwd: resolve(here, '..'),
      encoding: 'utf8',
    });
  } catch (error) {
    // npm exits non-zero when it finds anything; the report is still on stdout.
    if (error.stdout) return error.stdout;
    throw error;
  }
}

const report = JSON.parse(runAudit());
const found = Object.values(report.vulnerabilities ?? {});

/** Every advisory id or url npm reported, flattened across packages. */
const reported = new Set();
for (const vulnerability of found) {
  for (const via of vulnerability.via ?? []) {
    if (typeof via === 'object' && via.url) reported.add(via.url);
    if (typeof via === 'object' && via.source) reported.add(String(via.source));
  }
}

const acceptedIds = new Set(accepted.map((entry) => entry.id));
const isAccepted = (via) =>
  typeof via === 'object' && [...acceptedIds].some((id) => String(via.url ?? '').includes(id));

const blocking = [];
for (const vulnerability of found) {
  const vias = (vulnerability.via ?? []).filter((via) => typeof via === 'object');
  // A package is only excused when every advisory against it is excused.
  const unexcused = vias.filter((via) => !isAccepted(via));
  if (vias.length > 0 && unexcused.length > 0) {
    blocking.push({ name: vulnerability.name, severity: vulnerability.severity, vias: unexcused });
  }
}

const stale = accepted.filter((entry) => ![...reported].some((url) => url.includes(entry.id)));

for (const entry of accepted) {
  if (!stale.includes(entry)) {
    console.log(`accepted: ${entry.package} ${entry.id} — ${entry.ends_when}`);
  }
}

if (stale.length > 0) {
  console.error('\nThese exceptions are no longer reported and should be deleted:');
  for (const entry of stale) console.error(`  ${entry.package} ${entry.id} (${exceptionsPath})`);
  process.exit(1);
}

if (blocking.length > 0) {
  console.error('\nAdvisories with no accepted exception:');
  for (const item of blocking) {
    console.error(`  ${item.name} (${item.severity})`);
    for (const via of item.vias) console.error(`    ${via.title ?? via.name} ${via.url ?? ''}`);
  }
  console.error('\nFix them, or add a reviewed entry to audit-exceptions.json.');
  process.exit(1);
}

console.log(`\nno unreviewed advisories (${found.length} package(s) reported, all accounted for)`);
