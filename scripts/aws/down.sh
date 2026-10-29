#!/usr/bin/env bash
# Terminates the instance and deletes everything up.sh created. The index on the instance's
# local disks is gone afterwards: copy out anything you want to keep first.
set -euo pipefail
cd "$(dirname "$0")/../.."

export AWS_PROFILE=${AWS_PROFILE:-erickson}   # from `aws login --profile erickson`
export AWS_DEFAULT_REGION=${REGION:-us-east-1}
NAME=erickson
STATE=.aws

if [ -s "$STATE/instance" ]; then
  ID=$(cat "$STATE/instance")
  aws ec2 terminate-instances --instance-ids "$ID" >/dev/null 2>&1 || true
  echo "terminating $ID..."
  aws ec2 wait instance-terminated --instance-ids "$ID" 2>/dev/null || true
fi

SG=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || echo None)
[ "$SG" != "None" ] && aws ec2 delete-security-group --group-id "$SG" || true
aws ec2 delete-key-pair --key-name "$NAME" >/dev/null 2>&1 || true
aws iam remove-role-from-instance-profile --instance-profile-name "$NAME" --role-name "$NAME" 2>/dev/null || true
aws iam delete-instance-profile --instance-profile-name "$NAME" 2>/dev/null || true
aws iam delete-role-policy --role-name "$NAME" --policy-name commoncrawl-read 2>/dev/null || true
aws iam delete-role-policy --role-name "$NAME" --policy-name index-bucket-write 2>/dev/null || true
aws iam delete-role --role-name "$NAME" 2>/dev/null || true
rm -rf "$STATE"

LEFT=$(aws ec2 describe-instances --filters "Name=tag:project,Values=$NAME" "Name=instance-state-name,Values=pending,running,stopping,stopped" \
  --query 'length(Reservations[].Instances[])' --output text)
echo "done; erickson instances still alive: $LEFT"
