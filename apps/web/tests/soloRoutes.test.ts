import { describe, expect, it } from 'vitest';
import {
  claudeCodeHttpAddCommand,
  codexHttpSetupDryRunCommand,
  mcpEndpoint,
  setupClientDoctorCommand,
  setupClientHttpApplyCommand,
  setupClientHttpDryRunCommand,
} from '../src/lib/soloRoutes';

describe('solo route helpers', () => {
  it('derives MCP endpoints from Solo API URLs', () => {
    expect(mcpEndpoint('http://127.0.0.1:17821')).toBe('http://127.0.0.1:17821/mcp');
    expect(mcpEndpoint('http://127.0.0.1:17821/desktop/')).toBe('http://127.0.0.1:17821/mcp');
  });

  it('builds selector-free Community setup strings', () => {
    expect(codexHttpSetupDryRunCommand('http://127.0.0.1:17821')).toBe(
      'solo setup-client codex --scope user --transport http --url http://127.0.0.1:17821/mcp --dry-run',
    );
    expect(setupClientHttpDryRunCommand('claude-desktop', 'http://127.0.0.1:17821')).toBe(
      'solo setup-client claude-desktop --transport http --url http://127.0.0.1:17821/mcp --dry-run',
    );
    expect(setupClientHttpApplyCommand('codex', 'http://127.0.0.1:17821')).toBe(
      'solo setup-client codex --scope user --transport http --url http://127.0.0.1:17821/mcp --apply',
    );
    expect(setupClientHttpApplyCommand('claude-desktop', 'http://127.0.0.1:17821')).toBe(
      'solo setup-client claude-desktop --transport http --url http://127.0.0.1:17821/mcp --apply',
    );
    expect(setupClientDoctorCommand('http://127.0.0.1:17821')).toBe(
      'solo setup-client doctor --url http://127.0.0.1:17821/mcp',
    );
    expect(setupClientDoctorCommand('http://127.0.0.1:17821', 'codex')).toBe(
      'solo setup-client doctor codex --scope user --url http://127.0.0.1:17821/mcp',
    );
    expect(claudeCodeHttpAddCommand('http://127.0.0.1:17821')).toBe(
      'claude mcp add --transport http --scope user solo http://127.0.0.1:17821/mcp',
    );
  });
});
