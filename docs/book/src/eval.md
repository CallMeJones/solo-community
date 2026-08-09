# Eval Harness

Solo includes a small offline eval harness for memory-quality
regression checks. It is intentionally deterministic: fixtures are
local JSON files, scoring is lexical, and the command does not open a
Solo data directory, call the network, or download an embedding model.

Use it as a CI baseline and as a quick smoke test before swapping in
larger model-backed evals.

## Commands

List bundled fixtures:

```text
solo eval list
```

Run a bundled fixture:

```text
solo eval run memory-baseline --json
```

Save a report artifact for later inspection:

```text
solo eval run memory-baseline --json --save
solo eval report eval-1779999999999-memory-baseline
```

Run every bundled fixture for CI or release checks:

```text
solo eval run --all --json
```

Run a fixture file by path:

```text
solo eval run eval/fixtures/memory-corrections.json --json
```

The command exits non-zero if a fixture score is below its
`passing_score`; `--all` exits non-zero if any bundled fixture fails. The
`--json` output can be consumed directly by CI.

`--save` writes the JSON output to `.solo/eval-runs/<run-id>.json` by
default and adds `run_id`, `report_kind`, `saved_at_ms`, and
`report_path` to the JSON. Use `--report-dir <path>` with both `run` and
`report` when you want artifacts somewhere else:

```text
solo eval run --all --json --save --report-dir ./artifacts/eval-runs
solo eval report <run-id> --json --report-dir ./artifacts/eval-runs
solo eval report ./artifacts/eval-runs/<run-id>.json
```

## Fixture Shape

Fixtures live in `eval/fixtures/*.json` and are also embedded into the
CLI binary for the curated bundled set.

```json
{
  "name": "memory-baseline",
  "description": "Core memory recall cases.",
  "passing_score": 1.0,
  "cases": [
    {
      "id": "preference_lookup",
      "query": "What editor theme does Ada prefer?",
      "max_results": 3,
      "expected_memory_ids": ["pref-editor-theme"],
      "forbidden_memory_ids": ["stale-editor-theme"],
      "memories": [
        {
          "id": "pref-editor-theme",
          "text": "Ada prefers a dark editor theme.",
          "tier": "preference",
          "importance": 0.9,
          "status": "active"
        },
        {
          "id": "stale-editor-theme",
          "text": "Ada used to prefer a light editor theme.",
          "tier": "preference",
          "importance": 0.8,
          "status": "superseded"
        }
      ]
    }
  ]
}
```

Only `status: "active"` memories are eligible to rank. `forgotten` and
`superseded` fixture rows stay in the case as regression guards against
stale or unsafe memories winning.

`forbidden_memory_ids` is optional. Use it for stale, unsafe, private, or
ambiguous memories that must not appear in the top-k result set. A case
fails when any forbidden memory ranks inside `max_results` (or `--top-k`).

## Scoring

Each case ranks active memories with a deterministic lexical scorer:
query-token overlap, small tier and importance bonuses, and stable
tie-breaking by memory id. A case passes when every
`expected_memory_ids` entry appears within `max_results` (or `--top-k`
when supplied) and no `forbidden_memory_ids` entry appears in that same
result window. The fixture score is the average case score; a forbidden
hit scores the case as `0.0`.

This is not a replacement for semantic retrieval evaluation. It is a
cheap baseline that catches fixture drift, stale-memory precedence
mistakes, and obvious regressions without requiring models or network
access.
