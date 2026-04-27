#!/bin/bash
set -e

SQLCMD="/opt/mssql-tools18/bin/sqlcmd"
HOST="${MSSQL_HOST:-mssql}"
PASS="${SA_PASSWORD}"

echo "Waiting for SQL Server at $HOST..."
for i in $(seq 1 90); do
  $SQLCMD -S "$HOST" -U sa -P "$PASS" -C -Q "SELECT 1" >/dev/null 2>&1 && break
  if [ "$i" -eq 90 ]; then
    echo "ERROR: timed out waiting for SQL Server"
    exit 1
  fi
  sleep 2
done

echo "Creating testdb..."
$SQLCMD -S "$HOST" -U sa -P "$PASS" -C -Q "CREATE DATABASE testdb"

echo "Seeding tables..."
$SQLCMD -S "$HOST" -U sa -P "$PASS" -C -d testdb -i /seed.sql

echo "=== SEEDING COMPLETE ==="
