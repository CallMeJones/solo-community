#!/usr/bin/env node

import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const sourcePath = 'apps/web';
const sourceRoot = join(repoRoot, ...sourcePath.split('/'));
const distRoot = join(sourceRoot, 'dist');
const embeddedRoot = join(repoRoot, 'crates', 'solo-api', 'assets', 'solo-web');
const provenancePath = join(
  repoRoot,
  'crates',
  'solo-api',
  'assets',
  'solo-web.provenance.json',
);

function fail(message) {
  console.error(`embedded Web verification failed: ${message}`);
  process.exit(1);
}

function sha256File(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

function listFiles(root) {
  const files = [];
  const visit = (directory) => {
    for (const entry of readdirSync(directory).sort()) {
      const path = join(directory, entry);
      const stats = statSync(path);
      if (stats.isDirectory()) {
        visit(path);
      } else if (stats.isFile()) {
        files.push(path);
      }
    }
  };
  visit(root);
  return files;
}

function treeDigest(root) {
  const entries = listFiles(root).map((path) => {
    const name = relative(root, path).split(sep).join('/');
    return { name, digest: sha256File(path) };
  });
  entries.sort((left, right) =>
    left.name < right.name ? -1 : left.name > right.name ? 1 : 0,
  );
  const lines = entries.map(({ name, digest }) => `${name}:${digest}`);
  return createHash('sha256').update(lines.join('\n'), 'utf8').digest('hex');
}

function git(args) {
  return execFileSync('git', args, { cwd: repoRoot, encoding: 'utf8' }).trim();
}

for (const required of [
  join(sourceRoot, 'package-lock.json'),
  join(distRoot, 'index.html'),
  join(embeddedRoot, 'index.html'),
  provenancePath,
]) {
  if (!existsSync(required)) fail(`required file is missing: ${required}`);
}

const provenance = JSON.parse(readFileSync(provenancePath, 'utf8'));
if (provenance.schema_version !== 3) fail('provenance schema must be 3');
if (provenance.source_repository !== 'CallMeJones/solo-community') {
  fail(`unexpected source repository: ${provenance.source_repository}`);
}
if (provenance.source_path !== sourcePath) {
  fail(`unexpected source path: ${provenance.source_path}`);
}
if (!/^[0-9a-f]{40}$/.test(provenance.source_commit ?? '')) {
  fail('provenance source commit is not an exact lowercase Git SHA');
}
if (provenance.source_dirty !== false) fail('provenance records a dirty source');
if (!provenance.build_invocation_id) fail('provenance lacks a build invocation id');

const headCommit = git(['rev-parse', 'HEAD']);
const sourceCommitCheck = spawnSync(
  'git',
  ['cat-file', '-e', `${provenance.source_commit}^{commit}`],
  { cwd: repoRoot, stdio: 'ignore' },
);
if (sourceCommitCheck.status !== 0) {
  fail(`source commit is not reachable locally: ${provenance.source_commit}`);
}

const sourceDiff = spawnSync(
  'git',
  ['diff', '--quiet', provenance.source_commit, headCommit, '--', sourcePath],
  { cwd: repoRoot, stdio: 'ignore' },
);
if (sourceDiff.status === 1) {
  fail('apps/web changed after the embedded artifact source commit');
}
if (sourceDiff.status !== 0) fail('could not compare apps/web with its source commit');

const dirtySource = git([
  'status',
  '--porcelain=v1',
  '--untracked-files=all',
  '--',
  sourcePath,
]);
if (dirtySource) fail(`apps/web has uncommitted source changes:\n${dirtySource}`);

const packageLockDigest = sha256File(join(sourceRoot, 'package-lock.json'));
const rebuiltDigest = treeDigest(distRoot);
const embeddedDigest = treeDigest(embeddedRoot);
if (provenance.package_lock_sha256 !== packageLockDigest) {
  fail('package-lock digest does not match provenance');
}
if (provenance.dist_tree_sha256 !== rebuiltDigest || embeddedDigest !== rebuiltDigest) {
  fail(
    `artifact mismatch: rebuilt=${rebuiltDigest} embedded=${embeddedDigest} provenance=${provenance.dist_tree_sha256}`,
  );
}

console.log(
  JSON.stringify({
    source_repository: provenance.source_repository,
    source_path: sourcePath,
    source_commit: provenance.source_commit,
    head_commit: headCommit,
    package_lock_sha256: packageLockDigest,
    dist_tree_sha256: embeddedDigest,
  }),
);
