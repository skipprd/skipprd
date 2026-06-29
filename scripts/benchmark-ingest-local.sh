#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ROWS="${ROWS:-50000}"
FIXTURE="${FIXTURE:-wat_flat_exact}"
DATA_DIR="${DATA_DIR:-/tmp/skippr-ingest-benchmark}"
PROFILE_ROWS="${PROFILE_ROWS:-50000}"

mkdir -p "$DATA_DIR"
export DATA_DIR
export SKIPPR_CONFIG_FILE="$ROOT/tests/benchmark/ingest/skippr.yml"
export SKIPPR_PIPELINE=ingest_benchmark
export SKIPPR_WORKSPACE=benchmark
export DATA_DIR_MIN_FREE_BYTES=0
export DATA_DIR_HIGH_WATERMARK_PCT=100
export DATA_DIR_LOW_WATERMARK_PCT=100

CARGO=(./scripts/cargo-with-local-react.sh)

echo "==> Generating fixture ndjson (${FIXTURE}, rows=${ROWS})"
"${CARGO[@]}" test -p skipprd --lib ingest::benchmark_harness::tests::generates_fixture_files -- --nocapture >/dev/null

python3 - <<'PY' "$ROOT" "$FIXTURE" "$ROWS"
import json, pathlib, sys
root, fixture, rows = sys.argv[1:4]
rows = int(rows)
out = pathlib.Path(root) / "tests/benchmark/ingest/fixtures" / f"{fixture}.ndjson"
out.parent.mkdir(parents=True, exist_ok=True)

def wat_row(i):
    return {
        "crawl_id": "CC-MAIN-BENCH",
        "target_domain_hash_bucket": str(i % 32768),
        "target_domain_id": str(1_000_000 + (i % 10_000)),
        "target_domain": f"target-{i % 10000}.example",
        "source_url_id": str(2_000_000 + (i % 20_000)),
        "source_url": f"https://source-{i % 20000}.example/page",
        "source_domain_id": "100",
        "source_domain": "source.example",
        "source_host": "source.example",
        "warc_filename": "warc.warc.gz",
        "warc_record_offset": i,
        "warc_record_length": 1000,
        "wat_filename": "wat.wat.gz",
        "wat_record_offset": i,
        "wat_record_length": 500,
        "fetch_status": 200,
        "content_mime_type": "text/html",
        "fetch_time": "2026-01-01T00:00:00Z",
        "link_count_to_target": 1,
        "wat_path": f"s3://bucket/wat/{i}.wat.gz",
    }

def flat_dates_row(i):
    day = (i % 28) + 1
    hour = i % 24
    return {"id": f"row-{i}", "event_time": f"2026-01-{day:02d}T{hour:02d}:00:00Z", "value": i}

def fallback_row(i):
    row = flat_dates_row(i)
    if i % 97 == 0:
        row["unexpected_field"] = "force-fallback"
    if i % 211 == 0:
        row["value"] = "not-an-int"
    return row

def strings_heavy_row(i):
    pad = "x" * (180 + (i % 64))
    row = wat_row(i)
    row["source_url"] = f"https://source-{i % 20000}.example/{pad}/page?ref={pad}"
    row["wat_path"] = f"s3://bucket/wat/{i}/{pad}/wat.wat.gz"
    row["target_domain"] = f"target-{i % 10000}.{pad[:32]}.example"
    return row

def partition_skewed_row(i):
    bucket = i % 8
    return {
        "crawl_id": "CC-MAIN-BENCH",
        "target_domain_hash_bucket": str(bucket),
        "target_domain_id": str(1_000_000 + bucket),
        "target_domain": f"target-{bucket}.example",
        "source_url_id": str(2_000_000 + (bucket % 4)),
        "source_url": f"https://source-{bucket}.example/page",
        "source_domain_id": "100",
        "source_domain": "source.example",
        "source_host": "source.example",
        "warc_filename": "warc.warc.gz",
        "warc_record_offset": i,
        "warc_record_length": 1000,
        "wat_filename": "wat.wat.gz",
        "wat_record_offset": i,
        "wat_record_length": 500,
        "fetch_status": 200,
        "content_mime_type": "text/html",
        "fetch_time": "2026-01-01T00:00:00Z",
        "link_count_to_target": 1,
        "wat_path": f"s3://bucket/wat/{bucket}.wat.gz",
    }

generators = {
    "wat_flat_exact": wat_row,
    "flat_exact_dates": flat_dates_row,
    "fallback_mixed": fallback_row,
    "strings_heavy": strings_heavy_row,
    "partition_skewed": partition_skewed_row,
}

with out.open("w", encoding="utf-8") as fh:
    gen = generators[fixture]
    for i in range(rows):
        fh.write(json.dumps(gen(i)))
        fh.write("\n")
print(out)
PY

run_bench() {
  local name="$1"
  echo "==> ${name}"
  "${CARGO[@]}" test -p skipprd --lib "$2" -- --ignored --nocapture
}

run_bench "Legacy baseline (WAT, 10k)" bench_legacy_wat_flat_exact_baseline
run_bench "Default ingest (WAT, 10k)" bench_wat_flat_exact
run_bench "Date-heavy flat schema (10k)" bench_flat_exact_dates
run_bench "Long strings (10k)" bench_strings_heavy
run_bench "Partition-skewed WAT (10k)" bench_partition_skewed
run_bench "Fallback mix (2k)" bench_fallback_mixed_fixture

if command -v flamegraph >/dev/null 2>&1; then
  echo "==> Capturing flamegraph (WAT ${PROFILE_ROWS} rows, release build)"
  FIXTURE=wat_flat_exact ROWS="$PROFILE_ROWS" python3 - <<'PY' "$ROOT" "$PROFILE_ROWS"
import json, pathlib, sys
root, rows = sys.argv[1], int(sys.argv[2])
out = pathlib.Path(root) / "tests/benchmark/ingest/fixtures/wat_flat_exact.ndjson"
out.parent.mkdir(parents=True, exist_ok=True)
with out.open("w") as fh:
    for i in range(rows):
        fh.write(json.dumps({
            "crawl_id": "CC-MAIN-BENCH",
            "target_domain_hash_bucket": str(i % 32768),
            "target_domain_id": str(1_000_000 + (i % 10_000)),
            "target_domain": f"target-{i % 10000}.example",
            "source_url_id": str(2_000_000 + (i % 20_000)),
            "source_url": f"https://source-{i % 20000}.example/page",
            "source_domain_id": "100",
            "source_domain": "source.example",
            "source_host": "source.example",
            "warc_filename": "warc.warc.gz",
            "warc_record_offset": i,
            "warc_record_length": 1000,
            "wat_filename": "wat.wat.gz",
            "wat_record_offset": i,
            "wat_record_length": 500,
            "fetch_status": 200,
            "content_mime_type": "text/html",
            "fetch_time": "2026-01-01T00:00:00Z",
            "link_count_to_target": 1,
            "wat_path": f"s3://bucket/wat/{i}.wat.gz",
        }))
        fh.write("\n")
PY
  BENCHMARK_ROWS="$PROFILE_ROWS" flamegraph -o "$DATA_DIR/ingest-benchmark.svg" \
    --root \
    -- "${ROOT}/scripts/cargo-with-local-react.sh" test -p skipprd --release --lib bench_wat_flat_exact -- --ignored --nocapture
  echo "Flamegraph written to $DATA_DIR/ingest-benchmark.svg"
else
  echo "flamegraph not installed; skip SVG capture (brew install flamegraph)"
fi

echo "Done."
