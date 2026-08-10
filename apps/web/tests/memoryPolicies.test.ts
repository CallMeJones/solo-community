import { describe, expect, it } from 'vitest';
import { MEMORY_POLICY_TARGETS, renderMemoryPolicy } from '../src/lib/memoryPolicies';

describe('memory policy pack', () => {
  it('renders a copyable policy for every supported target', () => {
    for (const target of MEMORY_POLICY_TARGETS) {
      const policy = renderMemoryPolicy({
        target: target.value,
        libraryName: 'Community Memory Library',
        mcpUrl: 'http://solo.test/mcp',
      });

      expect(policy).toContain(
        target.label === 'Generic MCP Agent' ? 'Generic MCP Agent' : target.label,
      );
      expect(policy).toContain('Solo memory library: Community Memory Library');
      expect(policy).toContain('Solo MCP endpoint: http://solo.test/mcp');
      expect(policy).toContain('memory_context');
      expect(policy).toContain('Never store secrets');
    }
  });
});
