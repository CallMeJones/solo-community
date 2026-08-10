import { spawnSync } from 'node:child_process';

const env = {
  ...process.env,
  // Browser interception supplies deterministic E2E data. Never compile the
  // fixture data path into an artifact that CI may upload or Core may embed.
  VITE_SOLO_USE_MOCKS: '0',
};
const npmCli = process.env.npm_execpath;

if (!npmCli) {
  process.stderr.write('npm_execpath is unavailable; run this through npm run build:pilot.\n');
  process.exit(1);
}

for (const args of [
  ['run', 'build'],
  ['run', 'verify:pilot-artifact'],
]) {
  const result = spawnSync(process.execPath, [npmCli, ...args], { env, stdio: 'inherit' });
  if (result.error) {
    process.stderr.write(`${result.error.message}\n`);
  }
  if (result.status !== 0) process.exit(result.status ?? 1);
}
