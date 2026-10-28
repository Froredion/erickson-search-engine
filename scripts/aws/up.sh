#!/usr/bin/env bash
# Launch the indexing machine on AWS, copy this repo to it and build it there.
#
#   scripts/aws/up.sh             # i3en.12xlarge on-demand in us-east-1 (where Common Crawl lives)
#   TYPE=i3en.6xlarge scripts/aws/up.sh
#
# Creates, all named "erickson": an SSH key pair (saved to .aws/key.pem), a security group open
# to SSH from this machine's IP only, an IAM role that can read the Common Crawl bucket, and one
# instance. The instance terminates itself after MAX_HOURS (default 16) no matter what, so a
# forgotten machine can't run up the bill. scripts/aws/down.sh removes everything.
set -euo pipefail
cd "$(dirname "$0")/../.."

export AWS_DEFAULT_REGION=${REGION:-us-east-1}
TYPE=${TYPE:-i3en.12xlarge}
MAX_HOURS=${MAX_HOURS:-16}
NAME=erickson
STATE=.aws
mkdir -p "$STATE"

if [ -s "$STATE/instance" ] && aws ec2 describe-instances --instance-ids "$(cat "$STATE/instance")" \
    --query 'Reservations[0].Instances[0].State.Name' --output text 2>/dev/null | grep -qE 'pending|running'; then
  echo "already running: $(cat "$STATE/instance") at $(cat "$STATE/ip")"
  exit 0
fi

if [ ! -s "$STATE/key.pem" ]; then
  aws ec2 delete-key-pair --key-name "$NAME" >/dev/null 2>&1 || true
  aws ec2 create-key-pair --key-name "$NAME" --query KeyMaterial --output text > "$STATE/key.pem"
  chmod 600 "$STATE/key.pem"
fi

SG=$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" --query 'SecurityGroups[0].GroupId' --output text)
if [ "$SG" = "None" ]; then
  SG=$(aws ec2 create-security-group --group-name "$NAME" --description "erickson indexer" --query GroupId --output text)
fi
MY_IP=$(curl -fsS https://checkip.amazonaws.com)
aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 --cidr "$MY_IP/32" >/dev/null 2>&1 || true

if ! aws iam get-instance-profile --instance-profile-name "$NAME" >/dev/null 2>&1; then
  aws iam create-role --role-name "$NAME" --assume-role-policy-document \
    '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}' >/dev/null
  aws iam put-role-policy --role-name "$NAME" --policy-name commoncrawl-read --policy-document \
    '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:GetObject","s3:ListBucket"],"Resource":["arn:aws:s3:::commoncrawl","arn:aws:s3:::commoncrawl/*"]}]}'
  aws iam create-instance-profile --instance-profile-name "$NAME" >/dev/null
  aws iam add-role-to-instance-profile --instance-profile-name "$NAME" --role-name "$NAME"
  echo "waiting for the IAM role to propagate..."
  sleep 15
fi

AMI=$(aws ssm get-parameter --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
  --query Parameter.Value --output text)
ID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name "$NAME" \
  --security-group-ids "$SG" --iam-instance-profile "Name=$NAME" \
  --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=40,VolumeType=gp3,DeleteOnTermination=true}' \
  --instance-initiated-shutdown-behavior terminate \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$NAME},{Key=project,Value=$NAME}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "$ID" > "$STATE/instance"
echo "launched $ID ($TYPE); waiting for it to boot..."
aws ec2 wait instance-status-ok --instance-ids "$ID"
IP=$(aws ec2 describe-instances --instance-ids "$ID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "$IP" > "$STATE/ip"

SSH=(ssh -i "$STATE/key.pem" -o StrictHostKeyChecking=accept-new "ubuntu@$IP")
"${SSH[@]}" "sudo shutdown -h +$((MAX_HOURS * 60)) 'erickson cost cap reached'" 2>/dev/null || true
rsync -az --delete --exclude target --exclude data --exclude 'index*' --exclude .aws --exclude .git \
  -e "ssh -i $STATE/key.pem" ./ "ubuntu@$IP:erickson/"
"${SSH[@]}" "bash erickson/scripts/aws/setup.sh"

echo
echo "ready: $ID at $IP (self-terminates in $MAX_HOURS h)"
echo "  ssh -i $STATE/key.pem ubuntu@$IP"
echo "  start indexing:  scripts/aws/run.sh   (on the instance; see that file)"
