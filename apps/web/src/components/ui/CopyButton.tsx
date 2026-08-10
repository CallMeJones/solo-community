import { useEffect, useState } from 'react';

export function CopyButton({ label, value }: { label: string; value: string }) {
  const [status, setStatus] = useState<'idle' | 'copied' | 'failed'>('idle');
  const buttonLabel = status === 'copied' ? 'Copied' : status === 'failed' ? 'Copy failed' : label;

  useEffect(() => {
    if (status === 'idle') {
      return undefined;
    }
    const timeout = window.setTimeout(() => setStatus('idle'), 1600);
    return () => window.clearTimeout(timeout);
  }, [status]);

  async function copy() {
    try {
      if (!navigator.clipboard) {
        throw new Error('Clipboard API unavailable');
      }
      await navigator.clipboard.writeText(value);
      setStatus('copied');
    } catch {
      setStatus('failed');
    }
  }

  return (
    <button
      type="button"
      onClick={() => void copy()}
      className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
    >
      {buttonLabel}
    </button>
  );
}
