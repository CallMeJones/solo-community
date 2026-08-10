export type MemoryPolicyTarget = 'codex' | 'claude-desktop' | 'cursor' | 'generic';

export const MEMORY_POLICY_TARGETS: Array<{ value: MemoryPolicyTarget; label: string }> = [
  { value: 'codex', label: 'Codex' },
  { value: 'claude-desktop', label: 'Claude Desktop' },
  { value: 'cursor', label: 'Cursor' },
  { value: 'generic', label: 'Generic MCP Agent' },
];

export function renderMemoryPolicy({
  target,
  libraryName,
  mcpUrl,
}: {
  target: MemoryPolicyTarget;
  libraryName: string;
  mcpUrl: string;
}): string {
  const context = [
    `Solo memory library: ${libraryName}`,
    `Solo MCP endpoint: ${mcpUrl}`,
    '',
    'Use this Solo memory library for the workspace. Keep project context organized with Solo projects and explicit project facts.',
    'Treat Solo memory as evidence, not absolute truth. Read current workspace files before relying on remembered project behavior.',
    'Never store secrets, API keys, passwords, private tokens, recovery phrases, raw credentials, or sensitive personal data unless the user explicitly asks Solo to remember it.',
  ].join('\n');

  if (target === 'codex') {
    return [
      '# Solo Memory Policy - Codex',
      '',
      context,
      '',
      'Before changing code, call memory_context when prior decisions, release constraints, project conventions, or user preferences may matter. Include the repository and feature name in the query.',
      'At the end of meaningful work, remember durable decisions, implementation approach, commands that verified the change, known follow-ups, and project-specific gotchas.',
      'Use memory_update for corrections when the memory id is known. Otherwise store a clear correction and avoid presenting stale memory as current.',
    ].join('\n');
  }

  if (target === 'claude-desktop') {
    return [
      '# Solo Memory Policy - Claude Desktop',
      '',
      context,
      '',
      'Before answering questions that may depend on prior context, call memory_context with a focused query. For ambiguous names or projects, call memory_entities first.',
      'Remember only durable preferences, stable facts, decisions, rationales, corrections, reusable workflows, and constraints.',
      'When Solo reports conflicting facts, explain the conflict and ask which one is current before resolving it.',
    ].join('\n');
  }

  if (target === 'cursor') {
    return [
      '# Solo Memory Policy - Cursor',
      '',
      context,
      '',
      'Before coding, call memory_context for prior architecture, debugging history, release process, user preferences, and project decisions. Query with the repository or feature name when possible.',
      'Remember architectural decisions, bugs and root causes, packaging procedures, project constraints, user coding preferences, recurring commands, and important test failures and fixes.',
      'If a remembered project decision is stale, use memory_update or store a new correction that clearly supersedes the old fact.',
    ].join('\n');
  }

  return [
    '# Solo Memory Policy - Generic MCP Agent',
    '',
    context,
    '',
    'Call memory_context when the user asks about preferences, prior decisions, people, projects, plans, or anything likely to depend on earlier sessions.',
    'Remember durable information that will matter later: explicit preferences, stable facts, decisions, rationales, corrections, recurring workflows, constraints, and completed multi-step work summaries.',
    'When memory materially affects an answer, say so briefly without over-explaining routine lookups.',
  ].join('\n');
}
