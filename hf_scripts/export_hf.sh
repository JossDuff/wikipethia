#!/usr/bin/env bash
# Export wikipethia's documents table to a Hugging Face-ready Parquet file,
# using only the duckdb single binary (curl https://install.duckdb.org | sh).
#
# Same output as export_hf.py: one row per document, per-source license
# column, blank author/published as NULL. Aborts if any source lacks a
# license mapping rather than shipping rows with unknown terms.
#
# Usage: ./export_hf.sh [corpus.sqlite] [documents.parquet]
set -euo pipefail

DB="${1:-corpus.sqlite}"
OUT="${2:-documents.parquet}"

# Mirrors the licensing table in wikipethia's README.md.
LICENSE_CASE="
  CASE source
    WHEN 'ethresearch'    THEN 'CC-BY-NC-SA-3.0'
    WHEN 'ethmagicians'   THEN 'CC-BY-NC-SA-3.0'
    WHEN 'eips'           THEN 'CC0-1.0'
    WHEN 'ercs'           THEN 'CC0-1.0'
    WHEN 'consensusspecs' THEN 'CC0-1.0'
    WHEN 'executionspecs' THEN 'CC0-1.0'
    WHEN 'executionapis'  THEN 'CC0-1.0'
    WHEN 'pm'             THEN 'CC-BY-SA-3.0'
    WHEN 'vitalik'        THEN 'WTFPL'
    WHEN 'efblog'         THEN 'CC-BY-4.0'
  END"

duckdb <<SQL
INSTALL sqlite; LOAD sqlite;
ATTACH '$DB' AS corpus (TYPE sqlite, READ_ONLY);

-- Refuse to export a source with no license mapping.
SET VARIABLE unknown = (
  SELECT string_agg(DISTINCT source, ', ') FROM corpus.documents
  WHERE ($LICENSE_CASE) IS NULL
);
SELECT error('no license mapping for source(s): ' || getvariable('unknown') ||
             ' -- add them to this script and the README licensing table')
WHERE getvariable('unknown') IS NOT NULL;

COPY (
  SELECT
    id,
    source,
    ($LICENSE_CASE)        AS license,
    url,
    title,
    nullif(author, '')     AS author,
    nullif(published, '')  AS published,
    content,
    meta
  FROM corpus.documents
  ORDER BY source, id
) TO '$OUT' (FORMAT parquet, COMPRESSION zstd);

SELECT source, count(*) AS rows, any_value($LICENSE_CASE) AS license
FROM corpus.documents GROUP BY source ORDER BY rows DESC;
SQL

echo "wrote $OUT"
