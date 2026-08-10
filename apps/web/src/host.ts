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

export interface SoloWebHostDefinition {
  id: string;
  productName: string;
  tagline: string;
  capabilities?: readonly string[];
  routes?: readonly SoloWebRouteModule[];
  settingsModules?: readonly SoloWebSlotModule[];
  statusModules?: readonly SoloWebSlotModule[];
}

export interface SoloWebHost {
  readonly id: string;
  readonly productName: string;
  readonly tagline: string;
  readonly capabilities: readonly string[];
  readonly routes: readonly SoloWebRouteModule[];
  readonly settingsModules: readonly SoloWebSlotModule[];
  readonly statusModules: readonly SoloWebSlotModule[];
}

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
  const settingsModules = definition.settingsModules ?? [];
  const statusModules = definition.statusModules ?? [];
  validateModuleIds('route', routes);
  validateModuleIds('settings', settingsModules);
  validateModuleIds('status', statusModules);
  for (const route of routes) {
    if (CORE_ROUTE_ID_SET.has(route.id)) {
      throw new Error(`host route cannot replace a Core route: ${route.id}`);
    }
  }

  return Object.freeze({
    id: definition.id,
    productName: definition.productName,
    tagline: definition.tagline,
    capabilities: Object.freeze([...(definition.capabilities ?? [])]),
    routes: ordered(routes),
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
