# Design QA — unified Solo workspace

Status: passed for the implemented Community workspace and Windows desktop flow.

## Visual truth and comparison

Selected visual truth: `C:/Users/Michael/.codex/generated_images/01a0804b-e6ad-7bc1-adb0-18fe467adeca/exec-ef725d97-ac82-4bac-a9b9-06916ec406b6.png` (1487 × 1058 pixels).

Implementation evidence: `C:/Users/Michael/Desktop/solo/review-2026-09-08/new-ui/`:
- `focused-memory-final.png`: 1440 × 1024; selected Cedar memory and real neighborhood.
- `overview-2d-final.png`, `overview-3d-final.png`: 1440 × 1024; 10,000 synthetic memories in 20 stored groups.
- `settings-final.png`: 1440 × 1024; General settings.
- `mobile-final.png`, `mobile-graph-final.png`: 390 × 844; List and compact grouped graph.
- `native-device-settings-final.jpg`: bundled device controls reached from web Settings in the same window.
- `native-workspace-final.jpg`: 1442 × 1056 including Windows title bar; packaged native workspace after automatic unlock.

The selected reference and focused implementation were inspected together in one comparison input. Reference proportions were compared at the implementation width (1440/1487 = 0.968); its scaled height is approximately 1024. The reference includes a decorative title bar, while the browser captures start at app content and include a synthetic-preview notice. These framing differences were excluded from visual findings. Browser CSS viewports match the implementation image dimensions, with one image pixel per CSS pixel. Native title-bar chrome was assessed separately.

The user explicitly authorized changes to graph data presentation and shape for large libraries. The reference's nine hand-arranged example relationships are therefore not reproduced as fabricated facts: the implementation shows the fixture's actual memberships and links. The single-window layout, warm palette, Memories/List/2D/3D structure and conditional details panel carry the selected direction.

## Resolved findings

- P1: The Windows webview did not follow an in-page custom-protocol settings link. Mapped that exact internal link to Wry’s registered Windows origin; the packaged return path and restart now pass.

- P1: Desktop embedded the old web bundle despite a successful frontend build. Fixed through the repository's asset synchronization/provenance workflow and confirmed in the packaged executable.
- P2: Overview nodes and glow dominated the canvas. Reduced glow in the overview, separated groups and bounded node/label screen sizes.
- P2: Small focused graphs produced enormous circles after fitting. Capped screen-space node sizes while keeping labels readable.
- P2: 3D labels were tiny and groups overlapped in depth. Added fixed-size labels and a shallow overview, with full-depth neighborhoods and viewport-aware camera fitting.
- P2: A paged group could omit its anchor, leaving its first page disconnected. Prioritized the stored cluster anchor and verified its 99 member edges in a 100-item page.
- P2: Mobile navigation overflowed and graph controls covered the diagram. Added a five-column navigation row, four-group initial page and a separate group-selector row. Final 390px DOM check reports no overflowing controls.
- P2: Resizing could leave graph nodes outside the visible canvas. Refit on canvas size changes.
- P2: Details duplicated close controls and exposed the full correction form immediately. Removed the duplicate in the workspace, collapsed editing and used readable source/date labels.

## Required fidelity surfaces

- Typography: system sans-serif, strong 34px desktop heading, restrained secondary text, readable 13px graph labels. Labels retain a constant readable size as the graph zooms. The reference's larger decorative node icons are intentionally replaced by scale-aware data marks. Metadata is denser than the illustrative reference so operational detail fits in the inspector.
- Spacing/layout: 228px desktop navigation, shared header/view controls, flexible canvas and conditional details panel. At small widths navigation stays visible and details become a closable overlay. Group selector and paging controls remain outside the compact diagram.
- Colors/tokens: warm near-black background, off-white text and copper selection/action colors match the chosen direction. Existing selectable graph palettes and appearance preferences remain available. Overview bloom is suppressed for legibility.
- Image/icon quality: Phosphor supplies interface icons; the desktop uses Solo's supplied window icon. The graph is live data rendering, not a raster reproduction. No stock illustrations or placeholder imagery were introduced.
- Copy/content: navigation uses Memories, Inbox, Projects, Connected apps and Settings. Counts explicitly say items/groups; previews are labeled synthetic. No account, subscription or recovery capability is falsely presented as available.

At full size the header, sidebar, controls and inspector text were directly readable, so separate cropped image files were unnecessary; successive native/browser screenshots and accessibility trees provided focused inspection of those regions.

## Interaction evidence

- Saved a memory through the actual local API, corrected it, and confirmed the corrected text after reload.
- Switched List/2D/3D, selected a group and memory, opened its neighborhood, and loaded 100 additional records (100 → 200 of 501).
- Unit coverage verifies filtering, overlapping membership, preservation of original data, cross-group evidence and explicit document-section expansion.
- Live 10,000-memory fixture remains read-only. Production storage was never opened or modified for QA.
- Web suite: 241 passing, nine skipped. Native suite: 177 passing. Strict native lint and frontend type checks passed.

## Accepted differences and remaining limits

- No sign-in control: optional account services, subscription gating and encryption-key redesign are a separate implementation. Community remains fully usable offline.
- The graph still fetches the complete API graph before limiting rendering. The synthetic scale check does not establish a latency guarantee for every topology.
- 3D can still occlude nodes after a user rotates/expands a dense neighborhood; 2D, List, search, group selection and fit controls remain available.
- Windows was inspected live. macOS/Linux native behavior still requires platform CI/manual verification before release.
- Pro adapter routes remain supported in the shared host contract, but the Pro app has not been migrated or visually certified in this branch.

## Implementation checklist

- [x] Selected design and implemented UI compared together.
- [x] Desktop and compact layout inspected.
- [x] Large graph and real persistence workflows exercised.
- [x] Clean source linked to embedded assets by verified provenance.
- [x] Final native return and restart check (same window ID 918438).

Follow-up polish (P3): refine long source-group labels and add more keyboard graph navigation beyond the accessible list and group selector.
