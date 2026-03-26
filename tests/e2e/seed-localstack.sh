#!/usr/bin/env bash
set -euo pipefail
export AWS_ACCESS_KEY_ID=test
export AWS_SECRET_ACCESS_KEY=test
export AWS_DEFAULT_REGION=us-east-1
ENDPOINT="http://localstack:4566"

echo "Waiting for LocalStack..."
for i in $(seq 1 30); do
  curl -sf "$ENDPOINT/_localstack/health" &>/dev/null && break
  sleep 2
done

# S3
aws --endpoint-url "$ENDPOINT" s3 mb s3://test-bucket
aws --endpoint-url "$ENDPOINT" s3 cp ./tests/e2e/testdata/seed.json s3://test-bucket/test-data/seed.json

# DynamoDB
aws --endpoint-url "$ENDPOINT" dynamodb create-table \
  --table-name test_data \
  --attribute-definitions AttributeName=id,AttributeType=N \
  --key-schema AttributeName=id,KeyType=HASH \
  --billing-mode PAY_PER_REQUEST

# Load DynamoDB items from seed.json
while IFS= read -r line; do
  id=$(echo "$line" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
  name=$(echo "$line" | python3 -c "import sys,json; print(json.load(sys.stdin)['name'])")
  value=$(echo "$line" | python3 -c "import sys,json; print(json.load(sys.stdin)['value'])")
  created_at=$(echo "$line" | python3 -c "import sys,json; print(json.load(sys.stdin)['created_at'])")
  category=$(echo "$line" | python3 -c "import sys,json; print(json.load(sys.stdin)['category'])")
  aws --endpoint-url "$ENDPOINT" dynamodb put-item \
    --table-name test_data \
    --item "{\"id\":{\"N\":\"$id\"},\"name\":{\"S\":\"$name\"},\"value\":{\"N\":\"$value\"},\"created_at\":{\"S\":\"$created_at\"},\"category\":{\"S\":\"$category\"}}"
done < ./tests/e2e/testdata/seed.json

# Kinesis
aws --endpoint-url "$ENDPOINT" kinesis create-stream --stream-name test_stream --shard-count 1
sleep 2
while IFS= read -r line; do
  aws --endpoint-url "$ENDPOINT" kinesis put-record \
    --stream-name test_stream \
    --partition-key "pk" \
    --data "$(echo -n "$line" | base64)"
done < ./tests/e2e/testdata/seed.json

# SQS
aws --endpoint-url "$ENDPOINT" sqs create-queue --queue-name test_queue
QUEUE_URL="$ENDPOINT/000000000000/test_queue"
while IFS= read -r line; do
  aws --endpoint-url "$ENDPOINT" sqs send-message \
    --queue-url "$QUEUE_URL" \
    --message-body "$line"
done < ./tests/e2e/testdata/seed.json

echo "LocalStack seeded successfully"
