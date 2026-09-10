import type { ReactNode } from 'react';

export const CORE_ROUTE_IDS = [
  'home',
  'setup',
  'health',
  'connections',
  'backups',
  'projects',
  'logs',
  'memories',
  'inbox',
  'import',
  'settings',
] as const;

export type CoreRouteId = (typeof CORE_ROUTE_IDS)[number];
export type AppRouteId = CoreRouteId | (string & Record<never, never>);

export interface SoloWebModuleContext {
  apiUrl: string;
  navigate: (routeId: AppRouteId) => void;
}

export interface SoloWebRouteModule {
  id: string;
  label: string;
  nav?: boolean;
  order?: number;
  render: (context: SoloWebModuleContext) => ReactNode;
}

export interface SoloWebSlotModule {
  id: string;
  order?: number;
  render: (context: SoloWebModuleContext) => ReactNode;
}

/**
 * A panel a host composition adds inside an existing Core view, as a tab
 * beside what is already there.
 *
 * This is deliberately not a route. A route is its own place in the sidebar,
 * for a subject of its own; a tab is more of the same subject. Sharing a
 * project's memory with other people belongs beside the project it is about,
 * not in a separate corner of the app that the reader has to connect back up
 * themselves.
 */
export interface SoloWebTabModule {
  id: string;
  label: string;
  order?: number;
  render: (context: SoloWebModuleContext) => ReactNode;
}

export interface SoloWebHostDefinition {
  id: string;
  productName: string;
  tagline: string;
  /**
   * What this composition calls its edition, shown beside the library status.
   *
   * Defaults to Community because that is what an undecorated composition is:
   * the whole free product, with nothing added. A downstream host that
   * composes paid modules on top says so here, so the one place the app names
   * an edition cannot contradict the modules it actually loaded.
   */
  editionLabel?: string;
  capabilities?: readonly string[];
  routes?: readonly SoloWebRouteModule[];
  /** Extra tabs inside the Projects view, beside the project itself. */
  projectTabs?: readonly SoloWebTabModule[];
  settingsModules?: readonly SoloWebSlotModule[];
  statusModules?: readonly SoloWebSlotModule[];
}

export interface SoloWebHost {
  readonly id: string;
  readonly productName: string;
  readonly tagline: string;
  readonly editionLabel: string;
  readonly capabilities: readonly string[];
  readonly routes: readonly SoloWebRouteModule[];
  readonly projectTabs: readonly SoloWebTabModule[];
  readonly settingsModules: readonly SoloWebSlotModule[];
  readonly statusModules: readonly SoloWebSlotModule[];
}

/** What an undecorated Solo composition is. */
export const DEFAULT_EDITION_LABEL = 'Community';

/**
 * The Projects view's own tab. Reserved so a host cannot quietly take the
 * place of the project itself the way it cannot replace a Core route.
 */
export const PROJECT_OVERVIEW_TAB_ID = 'project';

const MODULE_ID_PATTERN = /^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$/;
const CORE_ROUTE_ID_SET = new Set<string>(CORE_ROUTE_IDS);

function ordered<T extends { id: string; order?: number }>(modules: readonly T[]): readonly T[] {
  return Object.freeze(
    [...modules].sort((left, right) => (left.order ?? 100) - (right.order ?? 100) || left.id.localeCompare(right.id)),
  );
}

function validateModuleIds(kind: string, modules: readonly { id: string }[]): void {
  const seen = new Set<string>();
  for (const module of modules) {
    if (!MODULE_ID_PATTERN.test(module.id)) {
      throw new Error(`${kind} module id must use lowercase kebab-case: ${module.id}`);
    }
    if (seen.has(module.id)) {
      throw new Error(`duplicate ${kind} module id: ${module.id}`);
    }
    seen.add(module.id);
  }
}

/**
 * Defines a host composition without adding edition or licensing behavior to
 * the shared Web application. Downstream applications supply concrete modules;
 * the Community host intentionally supplies none.
 */
export function defineSoloWebHost(definition: SoloWebHostDefinition): SoloWebHost {
  if (!MODULE_ID_PATTERN.test(definition.id)) {
    throw new Error(`host id must use lowercase kebab-case: ${definition.id}`);
  }
  const routes = definition.routes ?? [];
  const projectTabs = definition.projectTabs ?? [];
  const settingsModules = definition.settingsModules ?? [];
  const statusModules = definition.statusModules ?? [];
  validateModuleIds('route', routes);
  validateModuleIds('project tab', projectTabs);
  validateModuleIds('settings', settingsModules);
  validateModuleIds('status', statusModules);
  for (const route of routes) {
    if (CORE_ROUTE_ID_SET.has(route.id)) {
      throw new Error(`host route cannot replace a Core route: ${route.id}`);
    }
  }
  for (const tab of projectTabs) {
    if (tab.id === PROJECT_OVERVIEW_TAB_ID) {
      throw new Error(`host project tab cannot replace the project itself: ${tab.id}`);
    }
  }

  return Object.freeze({
    id: definition.id,
    productName: definition.productName,
    tagline: definition.tagline,
    editionLabel: definition.editionLabel ?? DEFAULT_EDITION_LABEL,
    capabilities: Object.freeze([...(definition.capabilities ?? [])]),
    routes: ordered(routes),
    projectTabs: ordered(projectTabs),
    settingsModules: ordered(settingsModules),
    statusModules: ordered(statusModules),
  });
}

export const communityWebHost = defineSoloWebHost({
  id: 'community',
  productName: 'Solo',
  tagline: 'private memory and projects',
  capabilities: ['memory-library', 'projects', 'local-mcp'],
});
