#!/usr/bin/env python3
"""Export wikipethia's documents table to a Hugging Face-ready Parquet file.

Reads corpus.sqlite, joins in a per-source license column, and writes one
Parquet file with one row per document. Chunks, embeddings, and the FTS
index are deliberately excluded: they are derived artifacts of wikipethia's
own retrieval pipeline, not part of the corpus.

Usage:
    pip install pyarrow
    python3 export_hf.py [--db corpus.sqlite] [--out documents.parquet]
"""

import argparse
import sqlite3
import sys

import pyarrow as pa
import pyarrow.parquet as pq

# Mirrors the licensing table in wikipethia's README.md, which is sourced
# from each publisher's own statement. A source missing here aborts the
# export rather than shipping rows with an unknown license.
LICENSES = {
    "ethresearch": "CC-BY-NC-SA-3.0",
    "ethmagicians": "CC-BY-NC-SA-3.0",
    "eips": "CC0-1.0",
    "ercs": "CC0-1.0",
    "consensusspecs": "CC0-1.0",
    "executionspecs": "CC0-1.0",
    "executionapis": "CC0-1.0",
    "pm": "CC-BY-SA-3.0",
    "vitalik": "WTFPL",
    "efblog": "CC-BY-4.0",
}

SCHEMA = pa.schema(
    [
        pa.field("id", pa.string(), nullable=False),
        pa.field("source", pa.string(), nullable=False),
        pa.field("license", pa.string(), nullable=False),
        pa.field("url", pa.string(), nullable=False),
        pa.field("title", pa.string(), nullable=False),
        pa.field("author", pa.string()),
        pa.field("published", pa.string()),
        pa.field("content", pa.string(), nullable=False),
        pa.field("meta", pa.string(), nullable=False),
    ]
)

BATCH = 5000


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default="corpus.sqlite")
    ap.add_argument("--out", default="documents.parquet")
    args = ap.parse_args()

    db = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)

    unknown = [
        s
        for (s,) in db.execute("SELECT DISTINCT source FROM documents")
        if s not in LICENSES
    ]
    if unknown:
        print(
            f"error: no license mapping for source(s): {', '.join(unknown)}\n"
            "Add them to LICENSES (and the README licensing table) first.",
            file=sys.stderr,
        )
        return 1

    cur = db.execute(
        "SELECT id, source, url, title, author, published, content, meta"
        " FROM documents ORDER BY source, id"
    )

    total = 0
    per_source: dict[str, int] = {}
    writer = pq.ParquetWriter(args.out, SCHEMA, compression="zstd")
    try:
        while rows := cur.fetchmany(BATCH):
            cols: dict[str, list] = {name: [] for name in SCHEMA.names}
            for doc_id, source, url, title, author, published, content, meta in rows:
                cols["id"].append(doc_id)
                cols["source"].append(source)
                cols["license"].append(LICENSES[source])
                cols["url"].append(url)
                cols["title"].append(title)
                # Blank-but-present values become NULL so consumers can
                # filter on missingness instead of guessing at sentinels.
                cols["author"].append(author or None)
                cols["published"].append(published or None)
                cols["content"].append(content)
                cols["meta"].append(meta)
                per_source[source] = per_source.get(source, 0) + 1
            writer.write_batch(pa.record_batch(cols, schema=SCHEMA))
            total += len(rows)
    finally:
        writer.close()

    print(f"wrote {total} documents to {args.out}")
    for source in sorted(per_source, key=per_source.get, reverse=True):
        print(f"  {source:16} {per_source[source]:>6}  {LICENSES[source]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
