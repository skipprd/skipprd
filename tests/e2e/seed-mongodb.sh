#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Seeding MongoDB..."
docker exec -i "$(docker compose -f "$SCRIPT_DIR/../../docker-compose.yml" ps -q mongodb)" \
  mongosh --quiet --eval "
    db = db.getSiblingDB('skippr_test');
    db.test_data.drop();
    var docs = JSON.parse(cat('/dev/stdin'));
    if (Array.isArray(docs)) { db.test_data.insertMany(docs); }
    else { db.test_data.insertOne(docs); }
    print('Inserted ' + db.test_data.countDocuments() + ' document(s)');
  " < "$SCRIPT_DIR/testdata/seed.json"
