#!/usr/bin/env bash
# Runs on the instance, unattended: fetch a saved index from S3, measure query latency cold and
# warm, save the results next to the index, then shut down (the instance terminates on shutdown).
#
#   RUN=20261028-2352-18000 BUCKET=erickson-index-<account> LATENCY_QUERIES=/data/latency.txt \
#     nohup scripts/aws/measure.sh > /data/measure.out 2>&1 &
#
# Results land in s3://$BUCKET/$RUN/$NAME/ (default measure/): one eval log per pass (per-query ms
# in each line), index stats, peak memory, and STATUS written last. Each pass is cut off after
# PASS_LIMIT seconds so a slow index can't run past the instance's cost cap with nothing saved.
set -uo pipefail
cd "$HOME/erickson"
: "${BUCKET:?set BUCKET}" "${RUN:?set RUN}"
SRC="s3://$BUCKET/$RUN"
NAME=${NAME:-measure}
OUT=/data/$NAME
PASS_LIMIT=${PASS_LIMIT:-1800}
LATENCY_QUERIES=${LATENCY_QUERIES:-eval/queries.txt}
WARM_PASSES=${WARM_PASSES:-3}
mkdir -p "$OUT"
BIN=target/release/erickson
status=ok
[ -x /usr/bin/time ] || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq time >/dev/null

echo "== fetch index ($(date -u))"
start=$(date +%s)
GEN=$(aws s3 cp "$SRC/index/CURRENT" -) || status=failed
aws s3 sync "$SRC/index/$GEN" "/data/index/$GEN" --only-show-errors || status=failed
echo "$GEN" > /data/index/CURRENT
echo "fetched $(du -sh "/data/index/$GEN" | cut -f1) in $(( $(date +%s) - start ))s" | tee "$OUT/fetch.txt"

if [ "$status" = ok ]; then
  "$BIN" stats --index /data/index > "$OUT/stats.txt" 2>&1 || status=failed
  free -g > "$OUT/free.txt"
  # Cold: nothing of the index in the page cache, as on a node that just started.
  for set in web latency; do
    q=eval/queries.txt; [ "$set" = latency ] && q=$LATENCY_QUERIES
    sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
    timeout "$PASS_LIMIT" /usr/bin/time -v "$BIN" eval --index /data/index "$q" > "$OUT/$set-cold.txt" 2> "$OUT/$set-cold.time" || status=failed
    for i in $(seq 1 "$WARM_PASSES"); do
      timeout "$PASS_LIMIT" /usr/bin/time -v "$BIN" eval --index /data/index "$q" > "$OUT/$set-warm$i.txt" 2> "$OUT/$set-warm$i.time" || status=failed
    done
    echo "$set: $(tail -1 "$OUT/$set-cold.txt") cold; $(tail -1 "$OUT/$set-warm$WARM_PASSES.txt") warm"
  done
fi

aws s3 cp "$OUT" "$SRC/$NAME/" --recursive --only-show-errors || status=failed
aws s3 cp /data/measure.out "$SRC/$NAME/measure.out" --only-show-errors || true
echo "$status $(date -u)" | aws s3 cp - "$SRC/$NAME/STATUS" --only-show-errors || true
echo "== saved to $SRC/$NAME ($status); shutting down"
sudo shutdown -h now
