# Measurements

Everything here was measured on one AWS i3en.2xlarge (8 vCPU, 64 GB RAM, 2 × 2.5 TB NVMe, us-east-1)
unless it says otherwise, building from Common Crawl's CC-MAIN-2026-43 snapshot (October 2026)
with domain ranks from its June–August 2026 web graph. The scripts in `scripts/aws/` reproduce
each run; every instance shuts itself down when its job is done.

Query sets:
- **web set**: the 50 queries in `eval/queries.txt`, navigational and informational.
- **1,000-query set**: the first 1,000 questions of the MS MARCO passage dev set, run against the
  web index. They're real search questions, mostly long and natural-language, so they make a hard
  latency test.

"Warm" means the same pass run again with the index in the page cache; "cold" means caches
dropped first, as on a node that has just started.

## Scaling: 1M, 10M and 81M pages

The first 180, 1,800 and 18,000 WET files of the snapshot, built, deduplicated, merged into 16
segments and prior-ordered the same way (`scripts/aws/curve.sh`, `auto.sh`, `measure.sh`). Query
times are for the 1,000-query set.

| | 180 files | 1,800 files | 18,000 files |
|---|---|---|---|
| Live pages | 0.91M | 8.45M | 80.8M |
| Duplicates removed | 10.5% | 16.8% | 19.8% |
| Build (filter, index, dedup) | 3 min 41 s | 36 min 52 s | 6 h 4 min |
| WET files per second | 0.81 | 0.81 | 0.82 |
| Index size | 4.6 GB (5.0 GB per M) | 41.7 GB (4.9 GB per M) | 389 GB (4.8 GB per M) |
| Merge into 16 segments | 4 s, 3.4 GB RSS | 1 min 29 s, 16.8 GB | 12 min 20 s, 43 GB |
| Memory while serving | 2.1 GB | 5.9 GB | 21 GB |
| Scoring p50 / p99 | 2.2 / 6.4 ms | 6.8 / 29 ms | 30 / 164 ms |
| Rerank + snippets, mean | 26 ms | 26 ms | 26 ms |
| Warm p50 / p99 | 28 / 34 ms | 33 / 58 ms | ~57 / ~196 ms |
| Cold p50 / p99 | 81 / 165 ms | 120 / 214 ms | 181 / 331 ms |

Build time, index size and memory grow linearly with pages. Scoring grows a little slower than the
index (p99 ×4.5 for 9.3× the pages, then ×5.6 for 9.6×). Reranking is flat because it always sees
300 candidates; it's most of the time up to 10M, and scoring's tail dominates at 81M.

At 81M, the warm figures combine two runs: scoring at the default first-pass setting from one,
lookup and rerank from the other (the second run's cache was disturbed). The cold figures are
from a run at the previous first-pass setting.

## Getting 81M from 6 s to under 200 ms

The first 81M index answered the 1,000-query set at p50 6.3 s, p99 20.7 s. Each step below was
checked to return identical results to the step before (web set, 1,000-query set, exhaustive
scoring, the MS MARCO run file), except the last, which changes results on purpose.

| Step | 1,000-query set, warm p50 / p99 | Web set, warm p50 / p99 |
|---|---|---|
| First 81M index | 6.3 s / 20.7 s (cold) | 5.6 s / 11.6 s |
| Terms looked up once per segment, sparse dictionary in RAM | 1.7 s / 7.7 s (cold) | 1.7 s / 5.6 s (cold) |
| Page metadata held in RAM, dictionary sample on disk | 1.6 s / 6.0 s | 1.5 s / 3.5 s |
| 1,125 segments merged into 16 | 284 ms / 1.4 s | 115 ms / 0.94 s |
| Pages renumbered best-first, so the search stops early | 108 ms / 716 ms | 70 ms / 254 ms |
| Cursor order kept as 8-byte keys, plus a cheaper first pass for 5+ word queries (at 50%) | 53 ms / 182 ms | 64 ms / 137 ms |

The 81M index is 7× the machine's RAM. Until the merge, every query looked its terms up in each of
1,125 segments, thousands of random NVMe reads; after it, warm queries read nothing from disk and
the time is CPU in scoring.

### The cheaper first pass

For queries of five or more words, the first pass uses only the rarest words that together carry
60% of the query's weight; the 300 candidates it finds are then scored exactly with every word.
Common words ("how", "long", "take") have the longest posting lists and move a score least.
Against full scoring on the 81M index (1,000-query set):

| First pass keeps | Exact top 3 kept | Same #1 | Scoring p99 |
|---|---|---|---|
| 100% (full scoring) | 100% | 100% | 399 ms |
| 80% | 98.5% | 98.7% | 225 ms |
| 70% | 96.6% | 96.9% | 178 ms |
| **60% (default)** | **93.7%** | **94.5%** | **164 ms** |
| 50% | 89.6% | 90.7% | 151 ms |

`ERICKSON_FIRST_MASS` and `ERICKSON_FIRST_TERMS` override the setting, and `--exhaustive` scores
every posting with every word.

## Result quality

MS MARCO passage ranking (8.8M passages), self-reported on the public dev set and the TREC Deep
Learning 2019 and 2020 judgments. The DL figures predate the cheaper first pass; dev MRR@10 is at
the current default.

| | Dev MRR@10 | DL19 nDCG@10 | DL20 nDCG@10 |
|---|---|---|---|
| Erickson, keyword scoring only | 0.177 | 0.475 | 0.472 |
| Erickson, full ranking | 0.186 | 0.499 | 0.450 |
| Keyword baselines on the [leaderboard](https://microsoft.github.io/MSMARCO-Passage-Ranking-Submissions/leaderboard/) | 0.167–0.187 | 0.506 | 0.480 |
| Best neural entry (2022) | 0.463 | | |

That's the level of a well-tuned keyword engine and far from neural rankers; the next real gain
would be a learned reranker over the top results. MS MARCO is licensed for non-commercial research
and is used here only for measurement.

## Projected to 10B pages

Measured at 81M, projected to 10B. Not run.

10B pages is 100 nodes of 100M each.

### Latency of a 100-node fan-out

`examples/fanout.rs` simulates it from the measured per-query timings. One node's time for a query
is its measured scoring time scaled to the node's size (×1.24 for 100M), plus its measured lookup
and rerank, times run-to-run jitter measured between two machines (p1 / p99: 0.94 / 1.07), times a
per-node skew. Skew is how much more of a query's work one node's slice holds: lognormal with mean
1 and standard deviation σ. The query waits for its slowest node, plus 1 ms for the round trip
and merge. 1,000 queries × 20 trials, warm nodes:

| Layout | One node p50 / p99 | p99 at σ 0 | σ 0.1 | σ 0.2 |
|---|---|---|---|---|
| 1 × 81M (check: matches measured) | 58 / 196 ms | | | |
| 100 × 100M | 65 / 237 ms | 263 ms | 300 ms | 372 ms |
| 125 × 80M | 57 / 195 ms | 218 ms | 248 ms | 307 ms |
| 200 × 50M | 46 / 132 ms | 153 ms | 169 ms | 208 ms |

Skew isn't measured. With pages assigned to nodes by URL hash, every slice is a random sample, so
σ should be near 0–0.1. That puts 10B on 100 nodes of 100M at a p99 of about 260–300 ms. The fan-out
itself adds only ~10%; most of it is one 100M node's own tail.

### Cost

From the measurements above, at on-demand prices for i3en.2xlarge ($0.904/h):

- **Index:** 4.8 GB per million pages: 482 GB per 100M node, 48 TB in all.
- **Build:** 17,600 WET records per second per node. 81M live pages came from 384.5M records, so
  10B live pages need ~47.6B records: ~750 node-hours, about **$680**, plus ~$23 of merging.
- **Serving:** 100 nodes ≈ **$66,000 per month** on demand; reserved pricing is lower. RSS is 21 GB
  at 81M and warm queries read nothing from disk; at 100M that should still fit in 64 GB, not
  measured.
- **Throughput:** 0.31 vCPU-seconds per query at 81M (~0.37 at 100M): about 22 queries per second
  per node. Every query touches every node, so one set of 100 nodes serves ~22 queries per second
  (~$1.15 per 1,000 queries at full load); more traffic needs more replica sets.
- **Storage:** 48 TB in S3 is ~$1,100 per month in Standard, $600 in Standard-IA.

### What isn't built for 10B

- Deduplication across machines (today it's one table on one machine).
- Stable document IDs and a partition-to-node map.
- Query deadlines, partial results and replicas.
- Incremental updates (today an index is rebuilt as a whole).

One Common Crawl snapshot yields about 450M live pages, so 10B unique pages need many snapshots,
and how much they overlap after deduplication is unknown.
