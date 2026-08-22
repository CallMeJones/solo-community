export function mcpEndpoint(apiUrl: string): string {
  try {
    return new URL('/mcp', apiUrl).toString();
  } catch {
    return `${apiUrl.replace(/\/$/, '')}/mcp`;
  }
}

export type SetupClientTarget = 'codex' | 'claude-desktop' | 'cursor';

export function setupClientHttpCommand(
  target: SetupClientTarget,
  apiUrl: string,
  mode: 'dry-run' | 'apply',
): string {
  const base = ['solo', 'setup-client', target];
  if (target === 'codex') {
    base.push('--scope', 'user');
  }
  return [...base, '--transport', 'http', '--url', mcpEndpoint(apiUrl), `--${mode}`].join(' ');
}

export function setupClientHttpDryRunCommand(target: SetupClientTarget, apiUrl: string): string {
  return setupClientHttpCommand(target, apiUrl, 'dry-run');
}

export function setupClientHttpApplyCommand(target: SetupClientTarget, apiUrl: string): string {
  return setupClientHttpCommand(target, apiUrl, 'apply');
}

export function codexHttpSetupDryRunCommand(apiUrl: string): string {
  return setupClientHttpDryRunCommand('codex', apiUrl);
}

export function claudeCodeHttpAddCommand(apiUrl: string): string {
  return [
    'claude',
    'mcp',
    'add',
    '--transport',
    'http',
    '--scope',
    'user',
    'solo',
    mcpEndpoint(apiUrl),
  ].join(' ');
}

export function setupClientDoctorCommand(apiUrl: string, target?: SetupClientTarget): string {
  const base = ['solo', 'setup-client', 'doctor'];
  if (target) {
    base.push(target);
  }
  if (target === 'codex') {
    base.push('--scope', 'user');
  }
  return [...base, '--url', mcpEndpoint(apiUrl)].join(' ');
}
