# Backups & Recovery

Solo's data directory holds your entire memory store. This
chapter covers what to back up, how to restore, and what
recovery looks like when things go wrong.

## What's in a data directory

After `solo init` and a few `remember` calls, the data
directory looks like:

```text
.solo/
├── solo.db                      ← SQLCipher database
├── solo.db-wal                  ← SQLite WAL (only present mid-session)
├── solo.db-shm                  ← SQLite shared memory (only present mid-session)
├── solo.config.toml             ← salt + embedder identity
├── solo.lock                    ← runtime lockfile (don't back up)
├── hnsw_episodes.hnsw.data      ← live HNSW snapshot data
├── hnsw_episodes.hnsw.graph     ← live HNSW snapshot graph
├── hnsw_episodes_bak.hnsw.data  ← backup HNSW snapshot data
├── hnsw_episodes_bak.hnsw.graph ← backup HNSW snapshot graph
└── models/                      ← optional: BGE-M3 weights cache
    └── BAAI/bge-m3/
        ├── config.json
        ├── tokenizer.json
        └── model.safetensors
```

What each file contributes:

| file | role | recoverable? |
|---|---|---|
| `solo.db` | encrypted database — episodes, embeddings, clusters, abstractions, triples, contradictions | NO (the source of truth) |
| `solo.db-wal`, `solo.db-shm` | SQLite WAL state — uncommitted-to-main pages + shared-memory pointer state. Present only while Solo is running. | YES (folded into `solo.db` on clean shutdown / next open) |
| `solo.config.toml` | Argon2 salt + embedder identity | NO (without it, the passphrase can't derive the key) |
| `hnsw_episodes.hnsw.{data,graph}` | live HNSW snapshot for fast startup | YES (rebuilt from `embeddings` table on startup) |
| `hnsw_episodes_bak.hnsw.{data,graph}` | backup snapshot, used if live is corrupt | YES (same as live) |
| `solo.lock` | runtime mutex, present only while Solo is running | YES (ephemeral) |
| `models/...` | embedder weights | YES (re-download with `solo download-model`) |

## What to back up

The minimum-correct backup set:

```text
solo.db
solo.config.toml
```

The HNSW snapshot files are recoverable from `solo.db` — if
they're missing or corrupt, Solo rebuilds them on next
startup (slower than loading a snapshot, but correct). The
model weights are public files retrievable via `solo
download-model`.

If you want fast restores (no rebuild), include the snapshot
files:

```text
solo.db
solo.config.toml
hnsw_episodes.hnsw.data
hnsw_episodes.hnsw.graph
hnsw_episodes_bak.hnsw.data
hnsw_episodes_bak.hnsw.graph
```

Do **not** include `solo.lock` — it'll block startup if
restored on a different machine.

## How to back up

### `solo backup --to <path>` (recommended)

The simplest correct path:

```bash
solo backup --to /path/to/backup/solo-2026-05-07.db
```

This runs SQLite's online backup API through Solo, writing a
self-contained SQLCipher database encrypted with the same
Argon2id-derived key as your live data dir. The output file
is a valid Solo database — restore by copying it (and your
existing `solo.config.toml`) into a fresh data dir and
running `solo doctor` with the same passphrase.

  - **Lockfile semantics**: like every other one-shot,
    `solo backup` acquires `solo.lock`. If a daemon or
    another one-shot is running, the backup refuses with a
    clear error. Stop the daemon first, or use a different
    `--data-dir`.
  - **Destination collisions**: refuses to overwrite an
    existing file unless `--force` is passed.
  - **Same passphrase, same salt**: the backup is
    encrypted with the **same key** as the source. To open
    it on another machine you need both the passphrase and
    the source's `solo.config.toml` (which holds the salt).

The standard `sqlcipher` CLI does **not** work for this:
that uses PBKDF2 to derive a key from a passphrase, while
Solo uses Argon2id. The keys don't match, and `sqlcipher …
PRAGMA key = 'your-passphrase'; .backup …` fails with
"file is not a database." The `solo backup` subcommand
exists so you don't have to write your own raw-key
derivation script.

### Hot backup against a running daemon

When a `solo daemon` is running on the data dir, `solo
backup` (the CLI subcommand) refuses with a lockfile
conflict. Use the daemon's HTTP `POST /backup` endpoint
instead:

```bash
curl -X POST http://127.0.0.1:17821/backup \
  -H 'Content-Type: application/json' \
  -d '{"to": "/var/backups/solo/2026-05-07.db"}'
```

Response:

```json
{"path": "/var/backups/solo/2026-05-07.db", "elapsed_ms": 11}
```

The backup runs against the writer's existing connection
without taking the lockfile — the daemon keeps serving
reads and writes during the operation. SQLite's online
backup uses a page-level snapshot of the source taken at
backup-start time, so the result is consistent. Encryption
(Argon2id raw key) and restore semantics are identical to
the `solo backup` CLI path.

For LAN-bound deployments (`solo http-serve --bind <ip>
--bearer-token-file <path>`), include the bearer token:

```bash
curl -X POST http://10.0.0.5:17821/backup \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer <token>' \
  -d '{"to": "/var/backups/solo/2026-05-07.db", "force": true}'
```

Add `"force": true` if the destination file already exists
and you want to overwrite it. Without `force`, an existing
file returns 400.

The destination path is **server-side** — for LAN
deployments, the file lands on the daemon's filesystem,
not the curl client's. Move it off-host with `rsync` /
`scp` separately as your backup workflow requires.

See [HTTP API](./http-api.md) for the full endpoint
documentation including error codes.

The HNSW snapshot files (`hnsw_episodes.hnsw.{data,graph}`
+ `_bak` siblings) are safe to hot-copy — the writer saves
them via temp-name + atomic rename, so each filename always
points at a complete file.

### Cold backup (Solo stopped)

Easier: stop the daemon / MCP host, copy the files, restart.

```bash
# Stop the daemon (Ctrl+C in its terminal, or:
# pkill -f 'solo daemon')

cp -a /path/to/.solo /path/to/backup/

# Restart
solo daemon
```

The lockfile prevents Solo from running while the cold copy
is in progress, but waiting until the daemon is stopped is
cleaner than trying to coordinate.

### Encrypted off-site backups

`solo.db` is already encrypted at rest. For off-site
storage, you can either:

  - Trust SQLCipher's encryption and ship `solo.db` as-is.
    The passphrase is the key; an attacker with `solo.db`
    but no passphrase has nothing usable.
  - Add a second encryption layer (`gpg --symmetric`,
    `age`, etc.). Defense in depth — useful if your
    threat model includes a backup provider being
    compromised.

`solo.config.toml` is plaintext. It contains the Argon2
salt; an attacker with the salt + a guess at your
passphrase can attempt offline brute force. The salt makes
rainbow tables useless, but it doesn't slow down a targeted
attacker testing a known password list. If your
passphrase is weak, the salt isn't a substitute — pick a
strong one.

## How to restore

### Same machine

```bash
# Stop any running Solo
# Restore files
cp -a /path/to/backup/.solo /home/me/.solo

# Verify
solo doctor --data-dir /home/me/.solo
solo doctor --data-dir /home/me/.solo --with-stats   # opens the DB
```

If `--with-stats` succeeds with the right episode counts,
the restore is complete.

### Different machine

Three things need to come along:

  1. **The data files** (at minimum `solo.db` +
     `solo.config.toml`).
  2. **The passphrase** (in your head or password manager).
  3. **The matching Solo binary version**. v0.3.x can read
     v0.3.x data dirs without conversion. Cross-major
     restores may need migration; this hasn't been
     exercised yet (v0.3 is still the current major).

```bash
# On the new machine:
cargo binstall solo-cli                  # pre-built binary; same version family
mkdir -p ~/.solo
cp /path/from/backup/solo.db ~/.solo/
cp /path/from/backup/solo.config.toml ~/.solo/
# Optionally copy the snapshot files for faster startup

SOLO_PASSPHRASE='...' solo doctor --with-stats
```

If `cargo binstall` isn't available on the new machine, source
compile via `cargo install solo-cli` — on Windows from Git Bash
that needs the Strawberry Perl PATH prefix (msys's bundled Perl
bombs OpenSSL's `Configure`):

```bash
PATH="/c/Strawberry/perl/bin:/c/Strawberry/c/bin:$PATH" \
  cargo install solo-cli
```

If the active embedder uses BGE-M3, also copy the model
files (or `solo download-model` to fetch them on the new
machine).

### From a corrupt snapshot

If the live snapshot is corrupt but the backup snapshot is
intact, Solo automatically falls back to the backup at
startup:

```text
WARN live HNSW snapshot failed; trying .bak
INFO HNSW loaded from backup snapshot
```

If both are corrupt or missing, Solo rebuilds from the
`embeddings` table:

```text
WARN no HNSW snapshot available; starting fresh empty index.
The startup chain will attempt rebuild_hnsw_from_sql next
INFO rebuild_hnsw_from_sql: N vectors restored
```

The rebuild is slower than loading a snapshot (linear in
vector count) but produces an identical graph topology
modulo HNSW's randomised insertion order. Recall quality is
preserved.

### From `pending_index` after a crash

Solo's writer commits each new memory in two steps: write
into the SQL `embeddings` table, then `add` the vector into
the in-memory HNSW index. Between those two steps a row sits
in the `pending_index` outbox table. If the process crashes
in that window, the SQL row is durable but the HNSW index
hasn't seen it yet.

On startup, Solo replays `pending_index`:

```text
INFO pending_index replay applied at startup replayed=N
```

For each row in the outbox, the writer adds the vector to the
in-memory HNSW (idempotent — it checks membership first), then
deletes the row. The HNSW snapshot save (a separate, debounced
5-minute concern) doesn't enter this picture; recovery cares
about the in-memory index, not the on-disk snapshot.

This is automatic. No operator action needed.

## When recovery isn't possible

  - **Lost passphrase**: `solo.db` becomes unrecoverable.
    SQLCipher's encryption is designed to make this true.
    Restore from a backup with a passphrase you do know,
    or `solo init --force` to start fresh.
  - **`solo.config.toml` deleted, no backup**: the salt is
    lost. Solo can't derive the same key from the same
    passphrase without the salt. Same outcome as a lost
    passphrase. Always back up `solo.config.toml` alongside
    `solo.db`.
  - **`solo.db` corrupted beyond SQLite's recovery**: the
    SQLite `.recover` command sometimes salvages partial
    data, but the result needs to be re-encrypted by hand
    and there's no in-Solo path for this. Restore from a
    backup.
  - **Cross-major version skew that can't migrate**: not
    yet a real scenario; v0.3.x is current. Future major
    releases will document migration explicitly. Until then,
    keep your Solo install pinned to the version that
    wrote your data.

## Best-practice rotation

For a personal-scale data dir (single-digit GB):

  - **Daily** — automated cold copy to a second disk.
  - **Weekly** — encrypted off-site copy (cloud storage
    with `age` or `gpg`).
  - **Before major operations** (`solo reembed`, version
    upgrade, schema migration) — extra ad-hoc snapshot.

For high-value memory stores, treat backups as an explicit
operational responsibility, not a "set once and forget"
script. Practice a restore once per quarter to verify the
backups are actually readable.
