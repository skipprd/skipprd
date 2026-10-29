---
name: skippr-tpc
description: >-
  Run TPC-H Q1/Q3/Q6 against DuckDB, vanilla DataFusion, and skipprd query.
  GitHub is SF10. Laptop may SF1 with SKIPPR_TPC_LAPTOP=1; that is not a SHA
  substitute. Use when the user says /skippr-tpc, skippr-tpc, or TPC-H.
disable-model-invocation: true
---

# skippr-tpc

Opt-in. SHA battery is `.github/workflows/skippr-tpc.yml` **TPC-H SF10** (`tpcgen-cli tpch parquet -s 10`) Q1/Q3/Q6. Hash mismatch vs DuckDB or vanilla DataFusion fails. Do **not** run skipprd-docs. Do **not** publish. Do **not** write CloudCycle EU buckets. Do **not** put SF1 on precommit.

Laptop SF1 is iteration only and **not a SHA substitute**.

```bash
# laptop only
export SKIPPR_TPC_LAPTOP=1
uvx tpcgen-cli@4.0.0 tpch parquet -s 1 --output-dir /tmp/tpch-sf1
python3 tests/tpc/run.py --scale 1 --queries Q1,Q3,Q6
```

```bash
gh workflow run skippr-tpc.yml --ref "$(git branch --show-current)"
gh run watch --exit-status
```

Never skip cells. `continue-on-error` is illegal.
