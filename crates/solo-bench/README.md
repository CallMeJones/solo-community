# Solo Benchmarks

`solo-bench` contains reproducible quality and latency evaluation for Solo.
The first production harness measures session retrieval on the official
LongMemEval-S cleaned dataset. It does **not** claim end-to-end question-answer
accuracy.

No full-dataset quality score is included for this Community build. Run the
harness at the revision being evaluated and retain its evidence files.

## LongMemEval-S retrieval

Download the official 500-question dataset:

```bash
mkdir -p data/longmemeval
curl -L \
  https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json \
  -o data/longmemeval/longmemeval_s_cleaned.json
```

Run a five-question smoke with Solo's bundled `all-MiniLM-L6-v2` model:

```bash
cargo run --release -p solo-bench --features bundled-embedder --bin longmemeval -- \
  --data data/longmemeval/longmemeval_s_cleaned.json \
  --limit 5 \
  --run-label smoke
```

Run all 500 questions:

```bash
cargo run --release -p solo-bench --features bundled-embedder --bin longmemeval -- \
  --data data/longmemeval/longmemeval_s_cleaned.json \
  --run-label longmemeval-s-500
```

The default `--content user` concatenates only user turns into one Solo episode
per history session. This intentionally matches MemPalace's published raw
baseline corpus construction. `--content all` is a separate Solo-native
diagnostic and must not be compared to the MemPalace raw number without saying
that the indexed corpus changed.

The runner uses a fresh isolated SQLite/HNSW store for each question, calls
Solo's production hybrid recall function, and scores returned memory IDs
against exact `answer_session_ids`. The per-question database is an ephemeral
unencrypted scratch database. This
keeps the retrieval path production-identical while excluding SQLCipher setup
and persistent-store state from the quality comparison. It reports:

- `recall_any@K`: at least one evidence session appears in the top K;
- `recall_all@K`: every evidence session appears in the top K;
- standard binary-relevance `NDCG@K`;
- mean reciprocal rank;
- ingestion and query p50/p95/p99 latency;
- a JSONL row per question with the complete top-K ranking; and
- a machine-readable aggregate summary with dataset BLAKE3 and model identity.

Results default to `target/benchmarks/longmemeval/`. Runs using
`--embedder stub` are marked `publishable: false`; that mode exists only for
fast plumbing checks.

The committed one-question plumbing fixture can be run without loading a
model:

```bash
cargo run -p solo-bench --bin longmemeval -- \
  --data crates/solo-bench/tests/fixtures/longmemeval_smoke.json \
  --embedder stub \
  --run-label fixture-smoke
```

## Fair-comparison rules

1. Label retrieval recall separately from answer accuracy.
2. Publish the dataset digest, content mode, embedder identity, split, and
   complete per-question evidence JSONL.
3. Tune only on a declared development split. Inspect the held-out results
   once for a release candidate.
4. Do not compare `--content all` with a user-turn-only baseline.
5. Preserve misses in the evidence file; never silently skip failed questions.

The runner accepts the same `{ "dev": [...], "held_out": [...], "seed": 42 }`
split-file shape used by MemPalace:

```bash
cargo run --release -p solo-bench --features bundled-embedder --bin longmemeval -- \
  --data data/longmemeval/longmemeval_s_cleaned.json \
  --split-file path/to/lme_split_50_450.json \
  --subset dev \
  --run-label dev-50
```

The previous no-op `recall_p99` target was removed. The harness above measures
actual query latency on its declared corpus; it does not certify a 10K/100K/1M
scaling budget. Record the exact source commit and whether the checkout is
dirty beside any shared result. `publishable: true` only identifies a bundled
semantic-model run; it is not a quality threshold or proof of answer accuracy.
Provider degradation aborts the run rather than silently mixing retrieval modes.

`solo eval` remains a deterministic heuristic fixture check. Use this harness
to measure the production retrieval path; do not present the fixture score as
production recall accuracy.

The LongMemEval dataset and format are maintained by the benchmark authors:
<https://github.com/xiaowu0162/LongMemEval>.
