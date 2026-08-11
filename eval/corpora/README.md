# Retrieval gold corpus

`retrieval-v1.json` is the pre-change benchmark for Solo's bundled MiniLM +
BM25/RRF retrieval path. It is intentionally not part of the legacy lexical
`solo eval run --all` suite: several cases have little shared vocabulary and
would measure the lexical toy scorer rather than the production embedder.

`cargo test -p solo-cli --test retrieval_corpus` runs the fixture through the
real encrypted write, bundled MiniLM, HNSW, BM25, and RRF pipeline. Before
replacing or retuning MiniLM, record recall@k, MRR, top-result confusion,
latency, and vector-versus-lexical support. Keep the fixture and resulting report in
the change PR so a model swap is evidence-driven.
