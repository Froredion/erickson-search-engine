#!/usr/bin/env bash
# Runs on the instance, unattended: build the index (retrying with --resume if a pass fails),
# save it to S3, then shut down. The instance was launched with shutdown behaviour "terminate",
# so shutting down ends billing; the self-termination timer from up.sh stays as a backstop.
#
#   LIMIT=18000 BUCKET=erickson-index-<account> nohup scripts/aws/auto.sh > /data/auto.out 2>&1 &
#
# Results land in s3://$BUCKET/$RUN/: the index generation, CURRENT, the build log, the Linux
# binary that built it, and STATUS (ok or failed) written last.
set -uo pipefail
cd "$HOME/erickson"
: "${BUCKET:?set BUCKET}"
RUN=${RUN:-$(date -u +%Y%m%d-%H%M)-${LIMIT:-all}}
DEST="s3://$BUCKET/$RUN"
ATTEMPTS=${ATTEMPTS:-3}

status=failed
for i in $(seq 1 "$ATTEMPTS"); do
  echo "== build attempt $i/$ATTEMPTS ($(date -u))"
  if scripts/aws/run.sh >> /data/run.out 2>&1; then status=ok; break; fi
  echo "attempt $i failed; tail of the log:"; tail -5 /data/run.out
  sleep 30
done
echo "== build $status ($(date -u))"

if [ "$status" = ok ]; then
  GEN=$(cat /data/index/CURRENT)
  aws s3 sync "/data/index/$GEN" "$DEST/index/$GEN" --storage-class STANDARD_IA --only-show-errors || status=failed
  aws s3 cp /data/index/CURRENT "$DEST/index/CURRENT" --only-show-errors || status=failed
  aws s3 cp target/release/erickson "$DEST/erickson-linux-x86_64" --only-show-errors || true
fi
aws s3 cp /data/run.out "$DEST/run.out" --only-show-errors || true
aws s3 cp /data/auto.out "$DEST/auto.out" --only-show-errors || true
echo "$status $(date -u)" | aws s3 cp - "$DEST/STATUS" --only-show-errors || true
echo "== saved to $DEST ($status); shutting down"
sudo shutdown -h now
