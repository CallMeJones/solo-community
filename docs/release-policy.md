# Solo Community release policy

Public Solo Community releases must include every supported operating-system
variant before they are presented as a release.

For the current Community support matrix, that means:

- Windows x86-64 installer: `SoloSetup-<version>-x86_64.exe`;
- Windows x86-64 portable ZIP: `solo-cli-<version>-x86_64-pc-windows-msvc.zip`;
- Ubuntu 24.04 x86-64 desktop package:
  `solo-<version>-ubuntu24.04-amd64.deb`;
- checksums for every published artifact.

Linux-only or Windows-only artifacts may be retained as CI artifacts for
diagnostics, but they must not be published as a public GitHub Release unless
the tag and release notes explicitly say the artifact is platform-scoped and
not a Solo product release.

Use `vX.Y.Z-test.N` for cross-platform Community prereleases that do not publish
crates to crates.io. Use exact `vX.Y.Z` tags only for stable releases.
