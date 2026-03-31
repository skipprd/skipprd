#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ENDPOINT="http://127.0.0.1:14566"

echo "Seeding EventBridge via LocalStack..."

aws --endpoint-url "$ENDPOINT" events create-event-bus --name test_bus > /dev/null 2>&1 || true

QUEUE_URL=$(aws --endpoint-url "$ENDPOINT" sqs create-queue --queue-name test_eb_queue --query 'QueueUrl' --output text)
QUEUE_ARN=$(aws --endpoint-url "$ENDPOINT" sqs get-queue-attributes --queue-url "$QUEUE_URL" --attribute-names QueueArn --query 'Attributes.QueueArn' --output text)

aws --endpoint-url "$ENDPOINT" events put-rule \
  --name test_rule \
  --event-bus-name test_bus \
  --event-pattern '{"source":["skippr.test"]}' > /dev/null

aws --endpoint-url "$ENDPOINT" events put-targets \
  --rule test_rule \
  --event-bus-name test_bus \
  --targets "Id=sqs-target,Arn=$QUEUE_ARN" > /dev/null

while IFS= read -r line; do
  [ -z "$line" ] && continue
  aws --endpoint-url "$ENDPOINT" events put-events --entries "[{
    \"Source\": \"skippr.test\",
    \"DetailType\": \"test\",
    \"Detail\": $(echo "$line" | jq -Rs .),
    \"EventBusName\": \"test_bus\"
  }]" > /dev/null
done < "$SCRIPT_DIR/testdata/seed.json"

echo "EventBridge seeded: bus=test_bus queue=$QUEUE_URL"
