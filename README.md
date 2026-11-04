# Erickson

A web search engine over 81 million pages of the open web, written in Rust: crawl data in, ranked
results with snippets out, about 50 ms per query on one machine.

Everything between the raw crawl and the results page is in this repo: language and quality
filtering, exact and near-duplicate removal, the inverted index and its on-disk format, query
evaluation, ranking, snippets, the HTTP server and the search page.

## Run it

You need Rust (stable) and about 12 GB of disk. This builds a 1M-page index in a few minutes
(after the download); `wet 1800` instead gives about 10M pages.

```sh
cargo build --release
scripts/fetch.sh ranks                 # domain authority scores
scripts/fetch.sh wet 180               # 180 WET files ≈ 1M pages after filtering
./target/release/erickson ingest --index index --ranks data/ranks.tsv --files-per-segment 16 \
    data/CC-MAIN-2026-43/*.warc.wet.gz
./target/release/erickson merge --index index --segments 16
./target/release/erickson serve --index index --port 8080
```

Then open http://127.0.0.1:8080. `erickson` with no arguments lists the other commands (`query`,
`eval`, `bench`, `stats`). The scripts in `scripts/aws/` build and measure the large indexes on
AWS; each instance shuts itself down when its job is done.

To serve it publicly, run it behind a TLS proxy on the same machine (`--bind 127.0.0.1`, the
default). The server rate-limits each client IP, caps concurrent searches and caches repeated
queries.

## Numbers

Measured on one AWS i3en.2xlarge (8 vCPU, 64 GB RAM, NVMe), building from Common Crawl's October
2026 snapshot. Query times are over 1,000 real questions from a public query set, warm cache.

| | ~1M pages | ~10M pages | 81M pages |
|---|---|---|---|
| Build (filter, index, dedup) | 3 min 41 s | 36 min 52 s | 6 h 4 min |
| Index size | 4.6 GB | 41.7 GB | 389 GB |
| Memory while serving | 2.1 GB | 5.9 GB | 21 GB |
| Query p50 | 28 ms | 33 ms | ~57 ms |
| Query p99 | 34 ms | 58 ms | ~196 ms |

Build time, index size and memory grow linearly with the number of pages; query time grows a
little slower than that, because most of the index is skipped for any one query.

**Projected to 10B pages** (100 nodes of 100M each, from the measurements above, not run): about
$680 of compute to build, ~$66k/month to serve on demand, and a p99 of roughly 260–300 ms once a
query has to wait for the slowest of 100 nodes. How that's worked out is in
[PLAN.md](PLAN.md) and [examples/fanout.rs](examples/fanout.rs).

## How it works

```
Common Crawl WET files
  → filter       English only, drop boilerplate-heavy and low-quality pages
  → index        one segment per batch of files; postings in 128-document compressed blocks
  → dedup        exact and near-duplicate pages removed across the whole index
  → merge        16 large segments, pages renumbered best-first
  → search       skip every block that can't reach the top 300, rescore those with every term,
                 rerank with title, URL, host and proximity signals, then build snippets
```

Every segment is an independent index, so the same code runs one machine or a hundred: a bigger
deployment only changes how many segments there are and where they live.

## Third-party code

The libraries this builds on are listed in `Cargo.toml`; their license texts are in
[LICENSES/](LICENSES).

## License

MIT, see [LICENSE](LICENSE).
