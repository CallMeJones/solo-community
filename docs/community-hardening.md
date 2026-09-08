# Community trust and release hardening

This describes the changes under development after the 0.12.2 Community review.
It does not announce a new published release.

## HTTP request boundary

The HTTP router rejects unexpected hosts and browser origins before CORS or
route dispatch. In unauthenticated mode, an explicit request authority must
be `localhost` or a literal loopback IP. Foreign DNS names, malformed or duplicate
headers, and conflicting Host/URI authorities receive HTTP 403. Forwarded-host
headers cannot override the check. The guard also covers public probes and MCP
preflight requests.

Native clients may omit Origin. Authenticated deployments retain native remote
access and their own same-origin browser UI, while data routes still require the
configured bearer/OIDC credentials. Existing localhost browser origins remain
supported. Unauthenticated custom DNS aliases must switch to a literal loopback
address/localhost or use an authenticated deployment.

This closes the reproduced server-side DNS-rebinding gap. It is not an
end-to-end browser exploit certification or a replacement for authentication
when exposing a service beyond the local machine.

## Principal erasure

`solo gdpr forget` now serializes deletion through the library writer, alongside
asset ingestion and backups. It removes exclusively attributed document metadata,
retained original blobs, related asset records, and source-attributed memory
claims/reviews. Summaries containing erased episodes are removed for regeneration.
Graph relationships that lose all their evidence are removed; relationships
still supported by other memories retain only their surviving evidence. Episode
and document vectors are invalidated before the writer accepts another command.

An original owned or referenced by another principal causes a preflight error
before deletion. Resolve that shared ownership explicitly before retrying.
Unknown ownership is treated conservatively. The CLI reports document and asset
counts alongside the existing episode/chunk/triple counts.

SQL changes are transactional. Filesystem deletion is irreversible: an I/O or
commit failure can leave an incomplete erasure requiring retry. Missing blobs
are tolerated on retry; a successful result is never returned while a selected
retained original could not be removed. Call the synchronous storage entry point
from a blocking worker, as the CLI does.

Erasure covers attributed data in the active library. Imported source files
outside Solo, previous backups/exports, operator audit history, and legacy data
without usable attribution require separate review. This operation does not
promise physical secure wiping of storage media.

## Search during provider outages

Recall and document search fall back to lexical retrieval if query embedding
fails or exceeds five seconds. Existing ranking, limits, and forgotten-record
filters still apply. Index/configuration failures remain errors, and no different
embedding model is silently substituted.

Recall JSON and MCP structured results include `retrieval_mode` (`hybrid` or
`lexical_only`) and `warning`. Memory context reports a degraded recall section
while preserving other sections. MCP document search also includes this metadata.
The HTTP document-search body remains an array for compatibility; response
headers `X-Solo-Retrieval-Mode` and `X-Solo-Retrieval-Warning` expose degradation,
including when there are no matching chunks. These headers are CORS-exposed.

## Releases and evaluation

Windows candidates, Linux test packages, and stable publication call the full
Community CI workflow as a prerequisite at the same source revision. The
reusable suite has a separate concurrency group and read-only permissions.
Failed tests, dependency audit, or SDK packaging prevent downstream release jobs.
This does not prevent a repository administrator from manually uploading files;
repository-level release policy still matters.

The SDK version matches the 0.12.2 workspace. The locked `h2` dependency is updated
to 0.4.16 for RUSTSEC-2026-0258.

The existing LongMemEval harness is now part of Community; see
[benchmark instructions](../crates/solo-bench/README.md). Its smoke test exercises
the production HNSW/FTS/RRF pipeline. Bundled semantic-model runs are opt-in so
ordinary workspace tests do not download a model. Degraded retrieval aborts a
benchmark. The previous no-op p99 target is removed. The separate `solo eval`
command remains a heuristic fixture check and does not establish production
recall accuracy. No previous benchmark score is promoted to a result for this
new build.


## Legacy attribution review

Stop Solo and run `solo gdpr audit --data-dir <library>` with the library passphrase. The report counts all episodes/chunks and the subset without a principal, including blank ownership fields. It changes no records and does not infer the missing owner. Review source provenance individually; do not assign every legacy row to the current user or treat a zero count as proof that external copies were erased. Existing backups and exports need their own retention review.

## Client interoperability

The attachment tool advertises an object schema without a root `oneOf`: Claude rejected the entire tool catalog when that construct was present. Solo still requires exactly one of `doc_id` and `asset_id` before writing. Catalog tests check every input schema for this compatibility constraint.
