# wikipethia

Ethereum research corpus for LLMs.

Nine years of ethresear.ch, searchable locally and exposed over MCP, so a frontier model answers protocol questions from the actual literature.

## Why not just grep the text?

Search here is more than keyword matching, in three layers:

- **Ranked lexical search (FTS5 + BM25).** Keyword matches are scored: rare terms
  outweigh common ones, title/author hits outweigh body mentions, and
  stemming matches (ex: "exits" to "exit"). Exact tokens like `EIP-4844`
  or an author name hit precisely.
- **Semantic search (embeddings).** Every chunk of text is mapped by a small
  local model (BGE-small, via fastembed) to a point in vector space where
  *meaning*, not spelling, determines distance. A question about "PBS" finds
  the proposer/builder-separation posts that never uses the acronym.
- **Hybrid fusion (RRF).** Both rankings merge via reciprocal rank fusion:
  documents strong in either list surface, and exact-term hits are never
  diluted by the vector side.

Every result carries a stable doc id, author, published date, and URL.  The
date matters because Ethereum research supersedes itself and a 2019 design
post can flatly contradict a 2024 one.

## Help improve wikipethia by asking questions

A hand-written eval set of questions and answers (`tests/eval/questions.toml`)
scores every change by recall@10.  Feel free to contribute eval questions with
links to the source that you feel should be retrieved to answer the question.  

For example, if you wrote a post about LeanVM that you feel is the perfect source for a
question like "What assumptions does LeanVM make?", **add this to the eval set!!!**  It will
be a huge help in fine tuning wikipethia!
