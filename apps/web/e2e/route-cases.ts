export type RouteCase = {
  hash: string;
  texts: string[];
};

export const ROUTES: RouteCase[] = [
  { hash: 'home', texts: ['Home', 'Next actions', 'Solo status'] },
  { hash: 'setup', texts: ['Setup', 'Readiness', 'Start Solo'] },
  { hash: 'health', texts: ['Health', 'Daemon State', 'MCP Status', '0.12.0'] },
  { hash: 'connections', texts: ['Connections', 'Solo MCP', 'Memory Policy', '0.12.0'] },
  { hash: 'backups', texts: ['Backups', 'Hot Backup', 'Recovery Surface'] },
  { hash: 'projects', texts: ['Projects', 'Project Memory', 'Agent Policy'] },
  { hash: 'logs', texts: ['Logs', 'Diagnostics', 'tray.log'] },
  { hash: 'memories', texts: ['Memories', 'Memory library', 'Reset'] },
  { hash: 'inbox', texts: ['Memory inbox', 'Review queue', 'Contradictions'] },
  { hash: 'import', texts: ['Import', 'Source', 'Local path'] },
  { hash: 'settings', texts: ['Settings', 'Endpoints', 'Derived Memory & Triples', '0.12.0'] },
];
