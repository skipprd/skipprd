#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ENDPOINT="http://127.0.0.1:14566"

echo "Seeding SNS via LocalStack..."

TOPIC_ARN=$(aws --endpoint-url "$ENDPOINT" sns create-topic --name test_topic --query 'TopicArn' --output text)
QUEUE_URL=$(aws --endpoint-url "$ENDPOINT" sqs create-queue --queue-name test_sns_queue --query 'QueueUrl' --output text)
QUEUE_ARN=$(aws --endpoint-url "$ENDPOINT" sqs get-queue-attributes --queue-url "$QUEUE_URL" --attribute-names QueueArn --query 'Attributes.QueueArn' --output text)

aws --endpoint-url "$ENDPOINT" sns subscribe \
  --topic-arn "$TOPIC_ARN" \
  --protocol sqs \
  --notification-endpoint "$QUEUE_ARN" > /dev/null

while IFS= read -r line; do
  [ -z "$line" ] && continue
  aws --endpoint-url "$ENDPOINT" sns publish \
    --topic-arn "$TOPIC_ARN" \
    --message "$line" > /dev/null
done < "$SCRIPT_DIR/testdata/seed.json"

echo "SNS seeded: topic=$TOPIC_ARN queue=$QUEUE_URL"
