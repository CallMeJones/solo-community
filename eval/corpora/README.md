# Retrieval gold corpus

`retrieval-v1.json` is the pre-change benchmark for Solo's bundled MiniLM +
BM25/RRF retrieval path. It is intentionally not part of the legacy lexical
`solo eval run --all` suite: several cases have little shared vocabulary and
would measure the lexical toy scorer rather than the production embedder.

Before replacing or retuning MiniLM, run the corpus through the production
write/embed/index/recall pipeline and record recall@3, MRR, forbidden hits,
latency, and vector-only versus lexical-only ablations. Keep the fixture and
resulting report in the change PR so a model swap is evidence-driven.
