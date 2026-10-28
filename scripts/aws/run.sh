#!/usr/bin/env bash
# Runs on the instance: indexes a Common Crawl snapshot straight from S3 into /data/index.
# Rerunning after an interruption continues where it stopped.
#
#   nohup scripts/aws/run.sh > /data/run.out 2>&1 &          # the whole snapshot
#   LIMIT=900 nohup scripts/aws/run.sh > /data/run.out 2>&1 & # first 900 files (~1%) as a test
set -euo pipefail
cd "$HOME/erickson"

CRAWL=${CRAWL:-CC-MAIN-2026-43}
GRAPH=${GRAPH:-cc-main-2026-jun-jul-aug}
TOP_DOMAINS=${TOP_DOMAINS:-5000000}
mkdir -p /data/cc /data/work

if [ ! -s /data/cc/wet.paths ]; then
  aws s3 cp --only-show-errors "s3://commoncrawl/crawl-data/$CRAWL/wet.paths.gz" - | gunzip > /data/cc/wet.paths
fi
echo "$(wc -l < /data/cc/wet.paths) WET files in $CRAWL"

if [ ! -s /data/ranks.tsv ]; then
  # Sorted best-first, so this stops reading after TOP_DOMAINS lines.
  aws s3 cp --only-show-errors "s3://commoncrawl/projects/hyperlinkgraph/$GRAPH/domain/$GRAPH-domain-ranks.txt.gz" - 2>/dev/null \
    | gunzip 2>/dev/null \
    | awk -F'\t' -v top="$TOP_DOMAINS" 'NR > 1 { print $1 "\t" $5 } NR > top { exit }' > /data/ranks.tsv || true
fi
echo "$(wc -l < /data/ranks.tsv) ranked domains"

ARGS=(--index /data/index --ranks /data/ranks.tsv --paths /data/cc/wet.paths --fetch s3
      --files-per-segment 16 --work /data/work)
[ -n "${LIMIT:-}" ] && ARGS+=(--limit "$LIMIT")
compgen -G "/data/index/.tmp-gen-*" >/dev/null && ARGS+=(--resume)

start=$(date +%s)
./target/release/erickson ingest "${ARGS[@]}"
echo "finished in $(( ($(date +%s) - start) / 60 )) minutes"
du -sh /data/index/gen-* | tail -1
./target/release/erickson stats --index /data/index
