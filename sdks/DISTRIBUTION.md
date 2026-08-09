# Solo SDK Starter Distribution

Decision: keep the TypeScript and Python SDKs as repo-local copy-in starters
for now. Do not publish npm or PyPI packages until the registry release bar
below is met.

## Why

- The HTTP and MCP helper surface is still moving with the Desktop product.
- A registry package creates a support and semver contract before the API is
  ready.
- The current starters are dependency-free and useful as files users can copy
  into an agent project.
- Release bundles give users a clean download path without pretending the SDKs
  are stable runtime dependencies yet.

## Release Bundles

Build versioned zip bundles with:

```bash
python sdks/package_starters.py --out .smoke/sdk-starters --check
```

Default output:

- `solo-typescript-starter-<version>.zip`
- `solo-python-starter-<version>.zip`
- `solo-sdk-starters-<version>.zip`

Each bundle includes a `solo-starter-manifest.json` with
`"registry_publish": false`, the Solo version, and the expected file list.

Tag-triggered GitHub releases build the same bundles and attach them to the
release page. Manual `workflow_dispatch` publish resumes do not re-upload
release artifacts; rerun the tag-triggered `release-sdk-starters` job if only
the starter zips need to be replaced.

## Registry Release Bar

Promote to npm/PyPI only after all of these are true:

- HTTP and MCP helper method names have a documented semver policy.
- CI runs no-daemon starter smoke plus at least one daemon-backed read/write
  smoke on Windows and Linux.
- Package names, owners, license, readme, changelog, and deprecation policy
  are agreed.
- Secrets/auth guidance is clear and covered by tests.
- The release workflow can publish dry-run artifacts and verify installed
  packages from a clean project.

Until then, release the starter zip bundles and keep `sdks/typescript` private
in `package.json`.
