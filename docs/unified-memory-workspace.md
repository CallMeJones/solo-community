# Unified memory workspace

Solo Community now uses one desktop window for local unlock and the memory workspace. The normal `solo-tray` launch opens the bundled startup page and automatically navigates to `/desktop/#memories` after its own daemon is running and a fresh health check succeeds. `--legacy-controls` remains available for advanced operator recovery and custom development endpoints.

## Navigation and workflows

- Memories opens in List, with shared selection and search across List, 2D and 3D.
- Add memory and Edit memory use the existing local persistence APIs. Import, Inbox, Projects and Connected apps retain their existing operations.
- Settings provides General, Memory & intelligence, Advanced and All settings, plus setup, recovery, backup, diagnostics and logs shortcuts.
- In the native app, Device unlock & startup returns to the immutable bundled page in the same window. No passphrase is exposed to the HTTP workspace.
- Community works offline without an account. This change does not implement account registration, OAuth, subscription billing or account-derived encryption. Existing passphrases, backup recovery and opt-in OS-keychain storage remain in place.

## Graph behavior

The overview groups existing cluster memberships. Records without a cluster use clearly named source/type buckets. Overlapping memberships have deterministic display ownership, while the original graph and its relationships remain unchanged. Counts include structural records such as the cluster itself; the UI calls them items.

Overview links aggregate only actual cross-group relationships. Their tooltips report the number of stored relationships. The visual grouping never writes inferred facts or changes the library's clustering.

The initial desktop overview renders up to 60 groups (four on narrow screens). Every group is reachable through the group selector or incremental loading. Individual views start with 100 items and show explicit remaining counts. Group anchors are retained in the initial page so their edges remain visible. Focus shows a selected memory and direct neighbors; explicit expansion follows reachable nodes, including hidden document sections when requested.

2D uses stable group colors, screen-size label and node limits, label collision avoidance and fit/zoom controls. 3D uses a shallow overview to reduce occlusion, readable labels, and full-depth individual neighborhoods. Search filters actual results; source, age and type filters further narrow the view. The graph refits when the canvas changes size.

## Validation and practical limits

The web suite covers 1,000 and 10,000 memories, bounded rendering, complete reachability, unchanged source data, real cross-group edges, focused expansion, document sections and filtering. Live isolated-library checks cover saving and correcting a memory and seeing the correction after reload. Native checks cover trusted startup origins and strict IPC parsing; startup, restart and return navigation are checked in a disposable desktop library.

The graph API still loads the complete graph before applying display limits. The 10,000-memory fixture is a usability/data-integrity check, not a guarantee of performance for every graph topology or a server-side pagination implementation. Very large libraries will eventually need server-side graph queries and paging.

## Building the desktop assets

Commit changes under `apps/web`, then run `scripts/sync_solo_web_assets.ps1`. It builds that source, replaces the embedded asset tree, and records provenance. Verify with `node scripts/verify_embedded_web.mjs` before rebuilding the desktop/CLI binaries. Running the web build alone does not update the packaged desktop.
