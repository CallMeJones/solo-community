export type RouteCase = {
  hash: string;
  texts: string[];
};

export const ROUTES: RouteCase[] = [
  { hash: 'home', texts: ['Home', 'Next actions', 'Solo status'] },
  { hash: 'setup', texts: ['Setup', 'Readiness', 'Start Solo'] },
  { hash: 'health', texts: ['Health', 'Daemon State', 'MCP Status', '0.12.0'] },
  { hash: 'connections', texts: ['Connected apps', 'Solo MCP', 'Memory Policy', '0.12.0'] },
  { hash: 'backups', texts: ['Backups', 'Hot Backup', 'Recovery Surface'] },
  { hash: 'projects', texts: ['Projects', 'Project Memory', 'Agent Policy'] },
  { hash: 'logs', texts: ['Logs', 'Diagnostics', 'tray.log'] },
  // `Local library` is hidden below 760px, so it cannot stand in for the
  // mobile viewport this matrix also runs against.
  { hash: 'memories', texts: ['Memories', 'Filters', 'Import'] },
  { hash: 'inbox', texts: ['Memory inbox', 'Review queue', 'Contradictions'] },
  { hash: 'import', texts: ['Import', 'Source', 'Local path'] },
  // Settings opens on its General category; Endpoints and the steward panels
  // are present in the DOM but hidden behind the other tabs, so asserting on
  // them here checks nothing a reader of this list would expect.
  { hash: 'settings', texts: ['Settings', 'Appearance', 'Graph colors'] },
];
