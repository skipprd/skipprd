#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SEED_JSON="$SCRIPT_DIR/testdata/seed.json"
PYTHON_BIN="${PYTHON_BIN:-python3}"

echo "Waiting for MSSQL to be ready..."
for i in $(seq 1 60); do
  if "$PYTHON_BIN" - <<'PY'
import pytds

conn = pytds.connect(
    server="127.0.0.1",
    port=11433,
    database="master",
    user="sa",
    password="TestPass123!",
    cafile=None,
)
conn.close()
PY
  then
    break
  fi
  sleep 2
done

echo "Creating database and seeding data..."
SEED_JSON="$SEED_JSON" "$PYTHON_BIN" - <<'PY'
import json
import os
from pathlib import Path

import pytds

seed_path = Path(os.environ["SEED_JSON"])
rows = []
with seed_path.open() as handle:
    for line in handle:
        record = json.loads(line)
        rows.append(
            (
                int(record["id"]),
                record["name"],
                float(record["value"]),
                record["created_at"],
                record["category"],
            )
        )

master = pytds.connect(
    server="127.0.0.1",
    port=11433,
    database="master",
    user="sa",
    password="TestPass123!",
    cafile=None,
    autocommit=True,
)
with master.cursor() as cursor:
    cursor.execute(
        "IF DB_ID('skippr_test') IS NULL CREATE DATABASE skippr_test"
    )
master.close()

conn = pytds.connect(
    server="127.0.0.1",
    port=11433,
    database="skippr_test",
    user="sa",
    password="TestPass123!",
    cafile=None,
    autocommit=True,
)
with conn.cursor() as cursor:
    cursor.execute(
        """
        IF OBJECT_ID('dbo.test_data', 'U') IS NOT NULL
            DROP TABLE dbo.test_data
        """
    )
    cursor.execute(
        """
        CREATE TABLE dbo.test_data (
            id INT PRIMARY KEY,
            name NVARCHAR(255),
            value FLOAT,
            created_at DATETIME2,
            category NVARCHAR(50)
        )
        """
    )
    cursor.executemany(
        """
        INSERT INTO dbo.test_data (id, name, value, created_at, category)
        VALUES (%s, %s, %s, %s, %s)
        """,
        rows,
    )
conn.close()
PY

echo "MSSQL seeded successfully"
