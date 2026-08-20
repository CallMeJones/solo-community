// Appearance controls for the Settings page: theme, graph node palette, and
// the graph effects toggle.
//
// Every control applies immediately rather than behind a save step — the whole
// point is to judge a choice against the real UI — and persists via themeStore.

import { THEME_ORDER, THEMES, type ThemeId } from '../lib/theme';
import { NODE_KINDS } from '../lib/nodeKindTheme';
import { NODE_PALETTES, NODE_PALETTE_ORDER, type NodePaletteId } from '../lib/nodePalettes';
import { prefersReducedMotion, useActiveTheme, useThemeStore } from '../store/themeStore';

export function ThemePicker() {
  const theme = useThemeStore((s) => s.theme);
  const setTheme = useThemeStore((s) => s.setTheme);

  return (
    <div role="radiogroup" aria-label="Theme" className="mt-4 grid gap-2 sm:grid-cols-2">
      {THEME_ORDER.map((id) => (
        <ThemeCard key={id} id={id} selected={theme === id} onSelect={() => setTheme(id)} />
      ))}
    </div>
  );
}

export function NodePalettePicker() {
  const palette = useThemeStore((s) => s.nodePalette);
  const setPalette = useThemeStore((s) => s.setNodePalette);
  // Preview each palette on the surface the graph will actually render it on,
  // so a light theme shows the light variant.
  const scheme = useActiveTheme().colorScheme;

  return (
    <div role="radiogroup" aria-label="Graph colors" className="mt-4 grid gap-2 sm:grid-cols-2">
      {NODE_PALETTE_ORDER.map((id) => (
        <PaletteCard
          key={id}
          id={id}
          scheme={scheme}
          selected={palette === id}
          onSelect={() => setPalette(id)}
        />
      ))}
    </div>
  );
}

export function GraphEffectsToggle() {
  const effects = useThemeStore((s) => s.effects);
  const setEffects = useThemeStore((s) => s.setEffects);

  return (
    <div className="mt-4">
      <label className="flex cursor-pointer items-start gap-3 rounded-md border border-slate-700 bg-slate-900 px-3 py-2.5">
        <input
          type="checkbox"
          checked={effects}
          onChange={(e) => setEffects(e.target.checked)}
          className="mt-0.5 h-4 w-4 accent-sky-500"
        />
        <span className="min-w-0">
          <span className="block text-sm font-medium text-slate-100">Glow and motion</span>
          <span className="mt-0.5 block text-xs text-slate-400">
            Node bloom and animated flow along the connections. Turn off for a flat graph or
            on a slower machine.
          </span>
          {prefersReducedMotion() && effects && (
            <span className="mt-1 block text-xs text-amber-200">
              Your system asks for reduced motion. This was enabled manually.
            </span>
          )}
        </span>
      </label>
    </div>
  );
}

function ThemeCard({
  id,
  selected,
  onSelect,
}: {
  id: ThemeId;
  selected: boolean;
  onSelect: () => void;
}) {
  const { label, description, swatch } = THEMES[id];

  return (
    <OptionCard selected={selected} onSelect={onSelect} label={label} description={description}>
      <ThemeSwatch swatch={swatch} />
    </OptionCard>
  );
}

function PaletteCard({
  id,
  scheme,
  selected,
  onSelect,
}: {
  id: NodePaletteId;
  scheme: 'dark' | 'light';
  selected: boolean;
  onSelect: () => void;
}) {
  const { label, description } = NODE_PALETTES[id];
  const colors = NODE_PALETTES[id][scheme];

  return (
    <OptionCard selected={selected} onSelect={onSelect} label={label} description={description}>
      <span aria-hidden="true" className="flex shrink-0 items-center gap-1">
        {NODE_KINDS.map((kind) => (
          <span
            key={kind}
            className="h-4 w-4 rounded-full"
            style={{ backgroundColor: colors[kind], boxShadow: `0 0 6px ${colors[kind]}` }}
          />
        ))}
      </span>
    </OptionCard>
  );
}

function OptionCard({
  selected,
  onSelect,
  label,
  description,
  children,
}: {
  selected: boolean;
  onSelect: () => void;
  label: string;
  description: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={onSelect}
      className={[
        'flex items-center gap-3 rounded-md border px-3 py-2.5 text-left transition-colors',
        selected
          ? 'border-slate-500 bg-slate-800'
          : 'border-slate-700 bg-slate-900 hover:border-slate-600 hover:bg-slate-800',
      ].join(' ')}
    >
      {children}
      <span className="min-w-0">
        <span className="block text-sm font-medium text-slate-100">
          {label}
          {selected && <span className="ml-2 text-xs font-normal text-slate-400">active</span>}
        </span>
        <span className="mt-0.5 block truncate text-xs text-slate-400">{description}</span>
      </span>
    </button>
  );
}

/**
 * Literal colors, not utility classes — the swatch has to show what a theme
 * looks like while a *different* theme is active, so it must sidestep the
 * `[data-theme]` overrides that repaint everything else on the page.
 */
function ThemeSwatch({ swatch }: { swatch: readonly [string, string, string] }) {
  const [surface, panel, accent] = swatch;
  return (
    <span
      aria-hidden="true"
      className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md border border-black/25"
      style={{ backgroundColor: surface }}
    >
      <span
        className="flex h-6 w-6 items-center justify-center rounded"
        style={{ backgroundColor: panel }}
      >
        <span className="h-2.5 w-2.5 rounded-full" style={{ backgroundColor: accent }} />
      </span>
    </span>
  );
}
