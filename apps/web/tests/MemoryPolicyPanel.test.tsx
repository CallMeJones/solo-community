import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { MemoryPolicyPanel } from '../src/components/MemoryPolicyPanel';

describe('MemoryPolicyPanel', () => {
  it('shows the selected client policy with library and endpoint context', () => {
    render(
      <MemoryPolicyPanel
        libraryName="Community Memory Library"
        mcpUrl="http://solo.test/mcp"
      />,
    );

    expect(screen.getByRole('heading', { name: 'Memory Policy' })).toBeInTheDocument();
    expect(screen.getByText(/Solo Memory Policy - Codex/)).toBeInTheDocument();
    expect(screen.getByText(/Solo memory library: Community Memory Library/)).toBeInTheDocument();
    expect(screen.getByText(/Solo MCP endpoint: http:\/\/solo\.test\/mcp/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^copy policy$/i })).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Policy target'), {
      target: { value: 'claude-desktop' },
    });

    expect(screen.getByText(/Solo Memory Policy - Claude Desktop/)).toBeInTheDocument();
    expect(screen.getAllByText('Claude Desktop').length).toBeGreaterThan(0);
  });
});
