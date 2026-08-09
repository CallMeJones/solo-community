# ADR-0015: Community is a single Memory Library

**Status:** Accepted
**Date:** 2026-08-03
**Deciders:** Solo project
**Supersedes:** ADR-0004, ADR-0011
**Depends on:** ADR-0001, ADR-0002, ADR-0003

## TL;DR

| Concern | Decision |
|---|---|
| Community storage | Exactly one encrypted `solo.db` per data directory |
| Tenant management | Removed from Community, not hidden behind a flag |
| Project scopes | Unlimited, inside the one library |
| Multi-library | Private paid editions only, via a separate host |
| Enforcement | Release boundary test over every user-facing surface |

## Context

ADR-0004 established multi-tenancy through per-tenant SQLCipher files under
`<data_dir>/tenants/<id>.db`, with a `tenants_index.db` registry. ADR-0011
added per-tenant quotas and last-accessed eviction on top of it.

That design assumed Solo would ship one binary serving many isolated
libraries. The open-core split invalidated the assumption. `docs/editions.md`
v2 (2026-07-19) makes the public Core a complete single-library product and
moves multi-library management, entitlements, and paid coordination into the
private `solo-pro` composition, which consumes the public Core unchanged.

An earlier reading held that Community could hide tenant management in the UI
while leaving the tenant APIs callable, for the benefit of integrations and
future paid modules. That is rejected. Solo has no users whose multi-tenant
data requires compatibility, and a dormant multi-database implementation in an
Apache-2.0 binary is both a maintenance burden and a misleading product
boundary. Obscurity is not a boundary.

## Decision

Community opens exactly one `MemoryLibrary`, backed by one canonical SQLCipher
database at `<data_dir>/solo.db`. Supporting files (vector index, documents,
retained asset blobs, logs, backups) may exist, but no supported interface
creates a second user-selectable memory database.

The following are absent from Community rather than disabled:

- the `/v1/tenants` route family and its OpenAPI definitions;
- the `X-Solo-Tenant` header, tenant auth claims, and any database-routing input;
- tenant CLI commands, `--tenant`, per-tenant backup/restore, and quota surfaces;
- any daemon flag or environment variable that selects a tenant or profile.

Projects remain the in-library context boundary and stay unlimited. A project
is not a profile, tenant, database, or entitlement.

Multi-library routing lives only in the private Pro host, which opens
additional Core instances under its own control directory. The public Core
exposes `MemoryLibrary` construction and route composition as generic hooks;
those hooks carry no paid implementation and are not licensing gates.

## Consequences

Existing pre-boundary installations carry the `tenants/` layout on disk.
Startup promotes a previous `tenants/default.db` to the single-library layout
(`crates/solo-storage/src/init.rs`). This migration is the compatibility
surface that matters, and it must be exercised against a real populated data
directory before any public release, not only against temporary fixtures.

Per-tenant quotas and last-accessed eviction (ADR-0011) no longer have a
Community subject. Whether the paid editions need an equivalent per-library
policy is deliberately left open; it should be decided against the Pro Library
Manager rather than inherited from this design.

Internal identifiers may still carry tenant-era names while the rename lands.
That is naming debt, not behaviour: it must never make multi-library operation
reachable. `docs/editions.md` §8 Phase 2 tracks the rename.

## Enforcement

`crates/solo-cli/tests/community_boundary.rs` is the gate. It crawls every CLI
help surface, the generated OpenAPI document, the shipped docs, SDKs, examples,
smoke scripts, and the embedded Web assets for a forbidden-term list, then runs
`solo init` and asserts the result is one `solo.db` with no `tenants/`
directory and no `tenants_index.db`. It runs in the Linux MSRV job and in both
platform certification jobs.

A boundary that is only documented is not a boundary; this test is what makes
the decision real.
