export function suggestedBackupPath(dataDir: string, now = new Date()): string {
  const root = dataDir.trim();
  if (!root) return '';
  return joinSoloPath(root, `solo-backup-${backupStamp(now)}.db`);
}

export function libraryDbPath(dataDir: string): string {
  if (!dataDir.trim()) return 'not reported';
  return joinSoloPath(dataDir, 'solo.db');
}

export function configPath(dataDir: string): string {
  if (!dataDir.trim()) return 'not reported';
  return joinSoloPath(dataDir, 'solo.config.toml');
}

function backupStamp(now: Date): string {
  const pad = (value: number) => String(value).padStart(2, '0');
  return [
    now.getFullYear(),
    pad(now.getMonth() + 1),
    pad(now.getDate()),
    '-',
    pad(now.getHours()),
    pad(now.getMinutes()),
    pad(now.getSeconds()),
  ].join('');
}

function joinSoloPath(root: string, child: string): string {
  const separator = root.includes('\\') && !root.includes('/') ? '\\' : '/';
  return `${root.replace(/[\\/]+$/, '')}${separator}${child}`;
}
