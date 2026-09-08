# LongMemEval-S retrieval baseline — 8 September 2026

Solo retrieved at least one answer-bearing session in its top five results for **441 of 500 questions (88.2%)**. It retrieved every required session for **351 of 500 (70.2%)**. These are session retrieval scores, not end-to-end question-answering accuracy.

| Metric | Result |
| --- | ---: |
| Any required session, top 1 | 70.6% |
| Any required session, top 5 | 88.2% |
| All required sessions, top 5 | 70.2% |
| Any required session, top 10 | 93.8% |
| All required sessions, top 10 | 85.2% |
| Mean reciprocal rank | 0.7822 |
| NDCG at 5 | 0.7422 |
| Query latency median / p95 / p99 | 11.6 / 15.7 / 23.7 ms |
| Total run time | 575.2 seconds |

## Reproduce and interpret

Source commit: `a540307` on Community 0.12.2, with the production embedding, HNSW and FTS/RRF recall pipeline. This is the baseline preceding the 0.12.3 client/onboarding/recovery changes, which do not change retrieval ranking. Windows x86-64, optimized release build, bundled `all-MiniLM-L6-v2` (384 dimensions). Model SHA-256: `afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1`.

[Official cleaned dataset](https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/tree/main), `longmemeval_s_cleaned.json`, BLAKE3 `cd766d50fe982186db24cea5d73ffaccdda7e0fc1e6eac52bc1318898b4ad7f2`. All 500 questions were scored; none skipped. The 13 questions containing repeated session IDs retain every occurrence in the corpus. Relevant IDs earn NDCG credit only on their first appearance; duplicate hits still occupy rank positions.

Each question used its own unencrypted scratch database. Only user turns were ingested; assistant-only information is therefore underrepresented. The weakest category was single-session assistant information (42.9% any-hit at 5), followed by preferences (60%). Multi-session all-hit at 5 was 68.4%; temporal all-hit at 5 was 54.9%. These are the next retrieval improvement targets. Evaluate full-dialogue ingestion and evidence completeness on a held-out split before tuning or advertising improvement.

This ran on a development workstation, not dedicated benchmark hardware. The numbers exclude application startup, model download, encryption setup, network hops and answer generation. Do not use them as a latency SLA or compare with another product's answer-generation score.

Use the instructions in [the benchmark harness](../../crates/solo-bench/README.md), with `--embedder bundled`, no question limit, and an explicit output directory. [Machine-readable summary](2026-09-08-longmemeval-summary.json). Per-question evidence is retained with the review artifacts; all per-question recall/NDCG values and aggregate scores were independently recomputed from the recorded ranks.
