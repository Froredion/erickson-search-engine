#!/usr/bin/env bash
# Runs on the instance, unattended: builds small indexes from the first N WET files of the same
# snapshot as the 100M run, merged and prior-ordered the same way, and measures each, so the 1M
# and 10M points of the scaling curve come from the same hardware as the 100M one. Then shuts down.
#
#   BUCKET=erickson-index-<account> LATENCY_QUERIES=/data/latency.txt [SIZES="180 1800"] \
#     nohup scripts/aws/curve.sh > /data/curve.out 2>&1 &
#
# Results land in s3://$BUCKET/curve-<date>/files-N/: build and merge timings (time -v), stats,
# index size, and one eval log per pass (cold, then warm at each first-pass mass). STATUS last.
# The indexes themselves aren't kept.
set -uo pipefail
cd "$HOME/erickson"
: "${BUCKET:?set BUCKET}" "${LATENCY_QUERIES:?set LATENCY_QUERIES}"
DEST="s3://$BUCKET/curve-$(date -u +%Y%m%d-%H%M)"
SIZES=${SIZES:-180 1800}
MASSES=${MASSES:-0.6 0.5}
BIN=target/release/erickson
status=ok
[ -x /usr/bin/time ] || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq time >/dev/null

# run.sh with LIMIT=0 would index everything; this only wants its wet.paths and ranks.tsv.
LIMIT=1 scripts/aws/run.sh > /dev/null 2>&1 || true
rm -rf /data/index /data/work

for n in $SIZES; do
  OUT=/data/curve/files-$n
  IX=/data/idx-$n
  mkdir -p "$OUT"
  echo "== $n files: ingest ($(date -u))"
  /usr/bin/time -v "$BIN" ingest --index "$IX" --ranks /data/ranks.tsv --paths /data/cc/wet.paths \
    --fetch s3 --files-per-segment 16 --work /data/work --limit "$n" > "$OUT/ingest.txt" 2> "$OUT/ingest.time" || status=failed
  echo "== $n files: merge ($(date -u))"
  /usr/bin/time -v "$BIN" merge --index "$IX" --segments 16 --keep 1 > "$OUT/merge.txt" 2> "$OUT/merge.time" || status=failed
  "$BIN" stats --index "$IX" > "$OUT/stats.txt" 2>&1 || status=failed
  du -s --block-size=1M "$IX/$(cat "$IX/CURRENT")" > "$OUT/size.txt"
  for set in web latency; do
    q=eval/queries.txt; [ "$set" = latency ] && q=$LATENCY_QUERIES
    sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
    /usr/bin/time -v "$BIN" eval --index "$IX" "$q" > "$OUT/$set-cold.txt" 2> "$OUT/$set-cold.time" || status=failed
    "$BIN" eval --index "$IX" "$q" > /dev/null 2>&1
    for m in $MASSES; do
      ERICKSON_FIRST_MASS=$m /usr/bin/time -v "$BIN" eval --index "$IX" "$q" > "$OUT/$set-warm-$m.txt" 2> "$OUT/$set-warm-$m.time" || status=failed
    done
    echo "$n $set: $(tail -1 "$OUT/$set-cold.txt") cold; $(tail -1 "$OUT/$set-warm-0.6.txt") warm"
  done
  rm -rf "$IX" /data/work
done

aws s3 cp /data/curve "$DEST/" --recursive --only-show-errors || status=failed
aws s3 cp /data/curve.out "$DEST/curve.out" --only-show-errors || true
echo "$status $(date -u)" | aws s3 cp - "$DEST/STATUS" --only-show-errors || true
echo "== saved to $DEST ($status); shutting down"
sudo shutdown -h now
