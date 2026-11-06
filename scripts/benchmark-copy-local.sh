#!/usr/bin/env bash
# COPY perf ladder: thesis → measure → compare.
#
# Theses (from CloudCycle-shaped 256k observe):
#   T1  High-cardinality keys (namespace×day×rmc) + eager 1-part compact
#       → tiny objects → PUT/IPC bound (not encode CPU).
#   T2  Source must accept multiple ingest tasks (File used and_wait → 1/N;
#       staging EU/US looked single-core — verify File AND mocked S3).
#   T3  Compact re-decode + re-encode is secondary once objects are tiny.
#
# Controls:
#   high_card  CloudCycle-like detail-type + day + rmc
#   low_card   single namespace/day/rmc (same rows, few keys)
#   SOURCE=file|s3   (s3 needs LocalStack/MinIO endpoint + AWS creds)
#
# Usage:
#   ROWS=8000 VARIANT=high_card SOURCE=file ./scripts/benchmark-copy-local.sh
#   ROWS=8000 VARIANT=high_card SOURCE=s3   ./scripts/benchmark-copy-local.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ROWS="${ROWS:-1000}"
VARIANT="${VARIANT:-high_card}"
SOURCE="${SOURCE:-file}"
WORK="${WORK:-/tmp/skippr-copy-bench/${SOURCE}_${VARIANT}_${ROWS}}"
BIN="${BIN:-$ROOT/target/debug/skipprd}"
MANIFEST_DIR="${SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR:-/tmp/skippr-copy-bench/manifests}"
S3_ENDPOINT="${S3_ENDPOINT:-http://127.0.0.1:14566}"
S3_BUCKET="${S3_BUCKET:-skippr-copy-bench}"
S3_PREFIX="${S3_PREFIX:-copy-bench/${SOURCE}_${VARIANT}_${ROWS}/}"
# Small batch_size so many S3 objects become many ingest tasks (staging shape).
S3_BATCH_SIZE_BYTES="${S3_BATCH_SIZE_BYTES:-200000}"

if [[ ! -x "$BIN" ]]; then
  echo "missing binary: $BIN (cargo build -p skipprd first)" >&2
  exit 1
fi
if [[ ! -d "$MANIFEST_DIR" ]]; then
  echo "missing plugin manifests: $MANIFEST_DIR" >&2
  exit 1
fi
case "$SOURCE" in
  file|s3) ;;
  *) echo "SOURCE must be file or s3 (got $SOURCE)" >&2; exit 1 ;;
esac

rm -rf "$WORK"
mkdir -p "$WORK/src" "$WORK/out" "$WORK/data"

python3 - <<PY
import json, pathlib
rows = ${ROWS}
variant = "${VARIANT}"
src = pathlib.Path("${WORK}/src")
namespaces = ["raw_docket", "cloudcycle_docket", "consistence_prediction", "binfra_user_auth"]
rmcs = ["cemex", "hanson"]
pad = "x" * 120

def emit(i):
    if variant == "low_card":
        ns, rmc, day = "cloudcycle_docket", "cemex", 1
    else:
        ns = namespaces[i % len(namespaces)]
        rmc = rmcs[i % len(rmcs)]
        day = (i % 28) + 1
    # one unwrapped / quote-only every 1000 rows (US failure shape)
    if i % 1000 == 0:
        return {"raw_docket": f'{{"ticket":{i}}}', "rmc": rmc}
    if i % 1000 == 1:
        return {
            "version": "0",
            "id": f"id-{i}",
            "detail-type": ns.replace("_", "-") if ns != "binfra_user_auth" else "binfra-user-auth",
            "source": "cube",
            "time": '"',
            "detail": {"rmc": rmc, "raw_docket": pad, "n": i},
        }
    detail_type = {
        "raw_docket": "raw_docket",
        "cloudcycle_docket": "cloudcycle-docket",
        "consistence_prediction": "consistence-prediction",
        "binfra_user_auth": "binfra-user-auth",
    }[ns]
    return {
        "version": "0",
        "id": f"id-{i}",
        "detail-type": detail_type,
        "source": "cube",
        "time": f"2026-10-{day:02d}T12:{(i % 60):02d}:00Z",
        "detail": {"rmc": rmc, "raw_docket": pad, "n": i},
    }

per_file = 1000
files = max(1, (rows + per_file - 1) // per_file)
for f in range(files):
    start = f * per_file
    end = min(rows, start + per_file)
    with open(src / f"part-{f:03d}.json", "w") as out:
        for i in range(start, end):
            out.write(json.dumps(emit(i), separators=(",", ":")) + "\n")
print(f"wrote {rows} rows variant={variant} files={files}")
PY

if [[ "$SOURCE" == "file" ]]; then
  cat > "$WORK/skippr.yml" <<YAML
skippr:
  workspace: test
  skipprd_el_storage_mode: local
pipelines:
  cube_copy:
    auto_approve: yes
    env: test
    type: copy
    data_source: data_sources.src
    data_sink: data_sinks.dst
    transform:
      flatten_events: false
      namespace_fields: detail-type
      batch_time_fields: time
      batch_time_unit: day
      time_partition_prefix: p_
      batch_partition_fields: detail.rmc
data_sources:
  src:
    File:
      path: $WORK/src
      format: json
data_sinks:
  dst:
    File:
      output_dir: $WORK/out
      format: jsonl
YAML
else
  export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-test}"
  export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-test}"
  export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-us-east-1}"
  export AWS_EC2_METADATA_DISABLED=true
  echo "==> uploading src/ to s3://${S3_BUCKET}/${S3_PREFIX} via ${S3_ENDPOINT}"
  aws --endpoint-url "$S3_ENDPOINT" s3 mb "s3://${S3_BUCKET}" 2>/dev/null || true
  aws --endpoint-url "$S3_ENDPOINT" s3 rm "s3://${S3_BUCKET}/${S3_PREFIX}" --recursive 2>/dev/null || true
  aws --endpoint-url "$S3_ENDPOINT" s3 sync "$WORK/src/" "s3://${S3_BUCKET}/${S3_PREFIX}"
  OBJ_COUNT=$(aws --endpoint-url "$S3_ENDPOINT" s3 ls "s3://${S3_BUCKET}/${S3_PREFIX}" | wc -l | tr -d ' ')
  echo "uploaded_objects=${OBJ_COUNT}"
  cat > "$WORK/skippr.yml" <<YAML
skippr:
  workspace: test
  skipprd_el_storage_mode: local
pipelines:
  cube_copy:
    auto_approve: yes
    env: test
    type: copy
    data_source: data_sources.src
    data_sink: data_sinks.dst
    transform:
      flatten_events: false
      namespace_fields: detail-type
      batch_time_fields: time
      batch_time_unit: day
      time_partition_prefix: p_
      batch_partition_fields: detail.rmc
data_sources:
  src:
    S3:
      s3_bucket: ${S3_BUCKET}
      s3_prefix: ${S3_PREFIX}
      endpoint_url: ${S3_ENDPOINT}
      region: us-east-1
      format: json
      batch_size_bytes: ${S3_BATCH_SIZE_BYTES}
data_sinks:
  dst:
    File:
      output_dir: $WORK/out
      format: jsonl
YAML
fi

export USE_LOCAL_PLUGIN_CODE=1
export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR="$MANIFEST_DIR"
export DATA_DIR="$WORK/data"
export DATA_DIR_MIN_FREE_BYTES=0
export DATA_DIR_HIGH_WATERMARK_PCT=0
export SKIPPR_PERF_PROFILE=yes

echo "==> sync source=${SOURCE} variant=${VARIANT} rows=${ROWS}"
START_NS=$(python3 -c 'import time; print(time.time_ns())')
set +e
"$BIN" --config "$WORK/skippr.yml" sync --log --pipeline cube_copy --once >"$WORK/run.log" 2>&1
EXIT=$?
set -e
END_NS=$(python3 -c 'import time; print(time.time_ns())')
WALL_MS=$(( (END_NS - START_NS) / 1000000 ))
echo "EXIT=${EXIT} wall_ms=${WALL_MS}"

python3 - <<PY
import gzip, json, re, statistics
from collections import Counter
from datetime import datetime
from pathlib import Path

def parse_ts(line):
    m = re.match(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)", line)
    return datetime.strptime(m.group(1), "%Y-%m-%dT%H:%M:%S.%fZ") if m else None

work = Path("${WORK}")
rows_want = ${ROWS}
log = (work / "run.log").read_text(errors="replace").splitlines()
compact_rows = []
copy_profile = sync_p = plugin_p = None
copy_ingests = []
queues = []
persists = []
final_compact_start = compact_done = None
for line in log:
    if "Queueing" in line and (t := parse_ts(line)):
        queues.append(t)
    if "Finalising: draining and stopping compactor" in line:
        final_compact_start = parse_ts(line)
    if "Finalising: compaction+cleanup finished" in line:
        compact_done = parse_ts(line)
    if "PERF_EVENT" not in line:
        continue
    try:
        j = json.loads(line.split("PERF_EVENT ", 1)[1])
    except Exception:
        continue
    p = j.get("phase")
    t = parse_ts(line)
    if p == "wal_compact":
        compact_rows.append(j.get("rows") or 0)
    elif p == "copy_profile":
        copy_profile = j
    elif p == "sync":
        sync_p = j
    elif p == "plugin_ipc":
        plugin_p = j
    elif p == "copy_ingest":
        copy_ingests.append(j)
    elif p == "wal_persist" and t:
        persists.append(t)
out_rows = 0
gz = 0
for p in (work / "out").rglob("*.jsonl.gz"):
    gz += 1
    out_rows += sum(1 for line in gzip.open(p, "rt") if line.strip())
print("--- sync summary (startup excluded from attribution) ---")
print("exit", ${EXIT}, "wall_ms", ${WALL_MS}, "out_rows", out_rows, "gz_objects", gz)
if sync_p:
    print("sync_ms", sync_p.get("elapsed_ms"), "(includes plugin spawn)")
if queues and persists:
    print(
        "ingest_wal_ms",
        round((persists[-1] - queues[0]).total_seconds() * 1000, 1),
        "first_queue→last_persist",
    )
if final_compact_start and compact_done:
    print(
        "compact_drain_ms",
        round((compact_done - final_compact_start).total_seconds() * 1000, 1),
    )
if copy_ingests:
    print(
        "copy_ingest batches",
        len(copy_ingests),
        "sum_ms",
        sum(j.get("elapsed_ms", 0) for j in copy_ingests),
        "decode",
        sum(j.get("decode_ms", 0) for j in copy_ingests),
        "key",
        sum(j.get("key_ms", 0) for j in copy_ingests),
        "ns",
        sum(j.get("ns_ms", 0) for j in copy_ingests),
        "partition",
        sum(j.get("partition_ms", 0) for j in copy_ingests),
        "time",
        sum(j.get("time_ms", 0) for j in copy_ingests),
        "encode",
        sum(j.get("encode_ms", 0) for j in copy_ingests),
        "wal_submit",
        sum(j.get("wal_submit_ms", 0) for j in copy_ingests),
    )
if compact_rows:
    print(
        "wal_compact n",
        len(compact_rows),
        "rows/obj min/med/mean/max",
        min(compact_rows),
        statistics.median(compact_rows),
        round(statistics.mean(compact_rows), 1),
        max(compact_rows),
    )
if copy_profile:
    # Prefer apply over spawn for sync-path plugin cost.
    interesting = {
        k: copy_profile[k]
        for k in [
            "ingest_batch_ms",
            "ingest_decode_ms",
            "ingest_key_ms",
            "ingest_ns_ms",
            "ingest_partition_ms",
            "ingest_time_ms",
            "ingest_encode_ms",
            "ingest_wal_submit_ms",
            "ingest_batches",
            "ingest_keys",
            "compact_put_ms",
            "compact_encode_ms",
            "compact_decode_ms",
            "objects",
            "avg_rows_per_object",
            "avg_wal_parts",
            "plugin_apply_ms",
            "plugin_spawn_ms",
        ]
        if k in copy_profile
    }
    print("copy_profile", json.dumps(interesting, sort_keys=True))
if plugin_p:
    print(
        "plugin_ipc apply_ms",
        plugin_p.get("apply_ms"),
        "spawn_ms",
        plugin_p.get("spawn_ms"),
        "acquires",
        plugin_p.get("acquires"),
    )
threads = re.findall(r"(\d+)/(\d+) active threads", "\n".join(log))
if threads:
    print("active_threads samples", Counter(threads).most_common(5))
print("log", work / "run.log")
if out_rows < rows_want * 0.9:
    raise SystemExit(f"row shortfall: {out_rows}/{rows_want}")
PY
