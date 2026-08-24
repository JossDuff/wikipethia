# Publishing the corpus to Hugging Face

Everything needed to (re-)publish the documents table as a Hugging Face
dataset. Manual by design for now — each upload is a deliberate snapshot,
not a pipeline stage.

The dataset is **documents only**: no chunks, no FTS index, no embeddings.
Chunking belongs to the consumer, and embedding vectors are only meaningful
to the exact model that produced them.

## One-time setup

1. A Hugging Face account, and a **write**-scoped access token from
   Settings → Access Tokens. The token is a credential — it lives in `hf`'s
   local login, never in this repo.
2. The `hf` CLI (standalone installer, no pip):

   ```bash
   curl -LsSf https://hf.co/cli/install.sh | bash
   hf auth login
   ```

3. Create the dataset repo once:

   ```bash
   hf repo create wikipethia-test --repo-type dataset
   ```

## Each upload

**1. Export.** From the repo root, against a freshly `update`d corpus:

```bash
python3 hf_scripts/export_hf.py          # needs pyarrow
```

Writes `documents.parquet` (gitignored) and prints per-source row counts.
It refuses to export a source that has no license mapping — when
`sources.toml` gains a source, add it to the script's `LICENSES` map and
to the counts table in the card, alongside the README licensing-table
duty the manifest already carries.

**2. Update `dataset_card.md`.** It is the dataset's landing page (uploads
as the repo's `README.md`). Refresh the snapshot date and the per-source
row counts the export just printed. The licensing table must stay in
agreement with the top-level README's.

**3. Upload.**

```bash
hf upload JossDuff/wikipethia-test documents.parquet --repo-type dataset
hf upload JossDuff/wikipethia-test hf_scripts/dataset_card.md README.md --repo-type dataset
```

**4. Check** the repo page: the dataset viewer renders the rows (takes a
minute to build), and the `load_dataset` snippet works.

Each upload is a new git revision on the HF side, so old snapshots stay
pinnable. Note the removals caveat in the card: a published snapshot cannot
see upstream deletions after its date — that is what re-uploading from a
fresh sync is for.
