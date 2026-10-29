#!/usr/bin/env bash
# Runs on the instance: indexes a Common Crawl snapshot into /data/index, fetching each WET file
# and deleting it once indexed. Rerunning after an interruption continues where it stopped.
#
#   LIMIT=18000 nohup scripts/aws/run.sh > /data/run.out 2>&1 &   # ~100M pages
#   LIMIT=40 nohup scripts/aws/run.sh > /data/run.out 2>&1 &      # quick test
#
# FETCH=s3 (default) reads the bucket directly: fastest, and free from us-east-1 where it lives.
# FETCH=https goes through data.commoncrawl.org and works from any region.
set -euo pipefail
cd "$HOME/erickson"

CRAWL=${CRAWL:-CC-MAIN-2026-43}
GRAPH=${GRAPH:-cc-main-2026-jun-jul-aug}
TOP_DOMAINS=${TOP_DOMAINS:-5000000}
FETCH=${FETCH:-s3}
BASE=https://data.commoncrawl.org
mkdir -p /data/cc /data/work

if [ ! -s /data/cc/wet.paths ]; then
  curl -fsSL --retry 5 "$BASE/crawl-data/$CRAWL/wet.paths.gz" | gunzip > /data/cc/wet.paths
fi
echo "$(wc -l < /data/cc/wet.paths) WET files in $CRAWL"

if [ ! -s /data/ranks.tsv ]; then
  # Sorted best-first, so this stops reading after TOP_DOMAINS lines.
  curl -fsSL "$BASE/projects/hyperlinkgraph/$GRAPH/domain/$GRAPH-domain-ranks.txt.gz" 2>/dev/null \
    | gunzip 2>/dev/null \
    | awk -F'\t' -v top="$TOP_DOMAINS" 'NR > 1 { print $1 "\t" $5 } NR > top { exit }' > /data/ranks.tsv || true
fi
echo "$(wc -l < /data/ranks.tsv) ranked domains"

ARGS=(--index /data/index --ranks /data/ranks.tsv --paths /data/cc/wet.paths --fetch "$FETCH"
      --files-per-segment 16 --work /data/work)
[ -n "${LIMIT:-}" ] && ARGS+=(--limit "$LIMIT")
compgen -G "/data/index/.tmp-gen-*" >/dev/null && ARGS+=(--resume)

start=$(date +%s)
./target/release/erickson ingest "${ARGS[@]}"
echo "finished in $(( ($(date +%s) - start) / 60 )) minutes"
du -sh /data/index/gen-* | tail -1
./target/release/erickson stats --index /data/index
