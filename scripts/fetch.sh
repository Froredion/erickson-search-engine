#!/usr/bin/env bash
# Download Common Crawl data into data/.
#   scripts/fetch.sh wet N [CRAWL]   the first N WET files of a crawl (~64 MB, ~20-30k pages each)
#   scripts/fetch.sh ranks [TOP]     the TOP best domains by link-graph harmonic centrality
# Both are resumable: files already downloaded are skipped.
set -euo pipefail
BASE=https://data.commoncrawl.org
cd "$(dirname "$0")/.."

case "${1:-}" in
  wet)
    N=${2:?how many files}
    CRAWL=${3:-CC-MAIN-2026-43}
    DEST=data/$CRAWL
    mkdir -p "$DEST"
    [ -s "$DEST/wet.paths" ] || curl -fsSL "$BASE/crawl-data/$CRAWL/wet.paths.gz" | gzip -dc > "$DEST/wet.paths"
    head -n "$N" "$DEST/wet.paths" | DEST="$DEST" BASE="$BASE" xargs -P 8 -n 1 sh -c '
      f="$DEST/$(basename "$1")"
      [ -s "$f" ] && exit 0
      curl -fsSL --retry 5 "$BASE/$1" -o "$f.part" && mv "$f.part" "$f" && echo "got $(basename "$1")"
    ' _
    ;;
  ranks)
    TOP=${2:-5000000}
    GRAPH=${GRAPH:-cc-main-2026-jun-jul-aug}
    mkdir -p data
    # The file is sorted best-first, so streaming stops after TOP lines instead of fetching all 2.4 GB.
    curl -fsSL "$BASE/projects/hyperlinkgraph/$GRAPH/domain/$GRAPH-domain-ranks.txt.gz" 2>/dev/null \
      | gzip -dc 2>/dev/null \
      | awk -F'\t' -v top="$TOP" 'NR > 1 { print $1 "\t" $5 } NR > top { exit }' > data/ranks.tsv || true
    echo "$(wc -l < data/ranks.tsv | tr -d ' ') domains -> data/ranks.tsv"
    ;;
  *)
    echo "usage: $0 wet N [CRAWL] | ranks [TOP]" >&2
    exit 2
    ;;
esac
