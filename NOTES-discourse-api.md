# ethresear.ch API reconnaissance

Probed 2026-07-28 with curl, one request per second, UA
`wikipethia-recon/0.1 (personal research corpus; jduff360@gmail.com)`.
Instance: Discourse **3.5.2** behind nginx, HTTP/2.

## TL;DR for M1

- Corpus size: **3,103 topics / 59,393 posts**. A full sync at 1 req/s is
  ~104 list pages + ~3,100 topic fetches + batch follow-ups for long threads —
  roughly an hour and a half of wall clock.
- Pagination terminates via `more_topics_url: null`, **not** an HTTP error.
- `raw` is **not** in any payload by default — append `include_raw=1`. It works
  on both `/t/{id}.json` and the `post_ids[]` batch endpoint, so raw markdown
  costs zero extra requests.
- Batch endpoint has no meaningful ID-count limit; the real cap is **URL
  length (~8 KB → HTTP 414)**. Batch ~100–200 IDs to stay far under it.
- No Discourse AI plugin — no server-side semantic search exists. Building our
  own is not redundant.

---

## /about.json

```json
{
  "version": "3.5.2",
  "stats": {
    "topics_count": 3103,
    "posts_count": 59393,
    "users_count": 7524,
    "topics_30_days": 32,
    "posts_30_days": 549
  }
}
```

Growth is slow (~550 posts/month), so incremental re-syncs are cheap.

## /latest.json?page=0 — pagination

```json
{
  "topic_list": {
    "per_page": 30,
    "more_topics_url": "/latest?no_definitions=true&page=1",
    "topics": [
      {
        "id": 8,
        "title": "Read this before posting",
        "posts_count": 12,
        "highest_post_number": 29,
        "created_at": "2017-08-17T22:57:31.812Z",
        "last_posted_at": "2026-07-22T02:46:15.292Z",
        "category_id": 3,
        "pinned": true,
        "tags": []
      }
    ]
  }
}
```

- 30 topics per page → ~104 pages for the full corpus.
- **Past the last page** (`page=5000`): HTTP **200** with
  `topics: []` and `more_topics_url: null`. Terminate on `more_topics_url ==
  null` (or empty `topics`), never on status code.
- Pinned topics ("Read this before posting") sort first on page 0 regardless of
  activity — don't treat page-0 order as strictly chronological.
- `/latest` sorts by recent activity, so an old topic with a new reply moves to
  page 0 — good for incremental sync (walk until `last_posted_at` predates the
  checkpoint), but it means a one-shot paged walk is not a stable snapshot.

## /t/{id}.json — topic detail (tested on 426, "Minimal Viable Plasma", 144 posts)

```json
{
  "id": 426,
  "posts_count": 144,
  "highest_post_number": 146,
  "chunk_size": 20,
  "post_stream": {
    "posts": ["... exactly 20 objects ..."],
    "stream": [1249, 1540, 1541, 1572, 1590, "... 144 ids total ..."]
  }
}
```

- `post_stream.posts` holds exactly the first `chunk_size` = **20** posts;
  `post_stream.stream` holds all **144** post IDs. Confirms the CLAUDE.md
  gotcha (the "~20" is exactly 20 on this instance).
- `highest_post_number` (146) > `posts_count` (144) → two posts deleted;
  `post_number` has gaps even though the visible stream is contiguous IDs.
- Post objects carry `accepted_answer` / `can_vote` fields from the
  `discourse-solved` and `discourse-topic-voting` plugins — harmless noise, but
  `accepted_answer` could be a useful ranking signal later.

## /t/{id}/posts.json?post_ids[]= — batching

```
GET /t/426/posts.json?post_ids[]=1735&post_ids[]=1745&...&include_raw=1
```

```json
{ "post_stream": { "posts": ["... one object per matched id ..."] } }
```

Empirical limits (tested against topic 426):

| IDs requested | URL bytes | Result |
|---|---|---|
| 124 | ~2.2 KB | 200, all 124 returned |
| 300 (144 real + 156 fake) | 5.1 KB | 200, the 144 real returned |
| 600 | 10.5 KB | **HTTP 414 URI Too Long** |
| 1044 | 18.5 KB | HTTP/2 framing error (connection-level reject) |

- The cap is **URL length (nginx ~8 KB)**, not an ID count. Use batches of
  **100–200 IDs** — even 5-digit IDs at 200/batch is ~4 KB.
- **Unknown/deleted IDs are silently dropped** — no error, no placeholder. So
  stream gaps cost nothing, but you must not assume `returned == requested`.
- Only 144-post threads exist at the top end of this corpus (largest thread
  found), so most topics need **zero or one** follow-up batch.

## raw vs cooked

Default payloads contain **only `cooked`** (rendered HTML). Three ways to get raw:

1. `?include_raw=1` on `/t/{id}.json` **and** on the `posts.json` batch
   endpoint — adds a `raw` field to every post object. **This is the one to
   use**; raw arrives in the same requests we already make.
2. `/posts/{post_id}.json` — single post, includes `raw`. Don't use: one
   request per post.
3. `/raw/{topic_id}/{post_number}` — plaintext raw, no JSON envelope. Handy for
   debugging, addressed by post_number not post id.

MathJax comparison (topic 7095, "Using polynomial commitments to replace state
roots", 11 inline math expressions in the OP):

raw:

```
To prove $f_1(x_1) = y_1$ ... $f_k(x_k) = y_k$, first let
$F = (f_1 - y_1) * \prod_{i \ne 1} (X - x_i) + ...$
```

cooked:

```html
To prove <span class="math">f_1(x_1) = y_1</span> …
<span class="math">F = (f_1 - y_1) * \prod_{i \ne 1} (X - x_i) + ...</span>
```

The LaTeX body survives in cooked but the `$` delimiters do not, and it's
buried in HTML. Raw keeps `$...$` intact — confirms "store raw".

Quote blocks in raw match the CLAUDE.md shape exactly:

```
[quote="vbuterin, post:1, topic:426"]
a confirm signature from each of the previous owners...
[/quote]
```

(in cooked they become `<aside class="quote" data-username="..." data-post="..."
data-topic="...">`).

## robots.txt and rate limiting

- `User-agent: *` disallows `/admin/`, `/search`, `/badges`, `/my`, `/g`,
  `/session`, tag listing pages, and RSS variants. **Everything M1 touches
  (`/latest.json`, `/t/*.json`, `/t/*/posts.json`) is allowed.** No
  `Crawl-delay`. Sitemap exists at `/sitemap.xml` (alternative discovery path
  if we ever want it).
- Note `/search` is disallowed — if we ever considered leaning on Discourse's
  own search endpoint, robots.txt says no. Reinforces building our own index.
- **No rate-limit headers on 200 responses** (no `X-RateLimit-*`,
  no `Retry-After`). Discourse's anonymous throttle is invisible until it
  fires; expect `429` with a `Retry-After` header and honor it. I did not
  deliberately trigger a 429. Responses do carry `x-request-id` and
  `x-runtime` (~20 ms typical — the server is not struggling).

## Discourse AI: not installed

- `/ai/tools` → **404** (plain Discourse 404 page).
- Homepage asset bundle lists exactly these plugins: `checklist`,
  `discourse-details`, `discourse-math`, `discourse-solved`,
  `discourse-topic-voting`. **No `discourse-ai`** → no server-side
  embeddings/semantic search. Our retrieval stack is not duplicating anything.

## CLAUDE.md "Discourse gotchas" — verdicts

| Gotcha | Verdict |
|---|---|
| `posts` holds only first ~20; full list in `stream` | **Confirmed** — exactly 20 (`chunk_size`), stream has all IDs |
| Store `raw`, not `cooked`; `$$...$$` survives raw | **Confirmed, with a correction**: `raw` is absent from every default payload. You must send `include_raw=1` (works on topic + batch endpoints). A client written to read `raw` from plain `/t/{id}.json` gets nothing. |
| Strip `[quote="user, post:N, topic:M"]` blocks | **Confirmed** — exact syntax seen in the wild |
| `stream` has gaps where posts were deleted | **Confirmed with nuance**: the `stream` ID list itself is dense; the gaps are in `post_number` (`highest_post_number` 146 vs `posts_count` 144). Also, requesting a deleted/unknown ID via `post_ids[]` is **silently dropped**, not an error. |
| Batch via `post_ids[]` for long threads | **Confirmed, limit clarified**: no practical ID-count cap; the limit is URL length (~8 KB → 414). Batch 100–200. |
| Stub posts pointing at HackMD/arXiv | Not tested this pass — parsing concern, revisit in M2 |
