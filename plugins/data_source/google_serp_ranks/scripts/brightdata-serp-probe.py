#!/usr/bin/env python3
"""Probe Bright Data SERP API for soft errors (empty / plain-text HTTP 200).

Reproduces the intermittent failure seen in prod keyword hub runs:
  HTTP 200, body_len=0 or body_preview="Error while processing request"

Usage:
  AWS_PROFILE=skippr-prod AWS_DEFAULT_REGION=eu-west-1 \\
    python3 plugins/data_source/google_serp_ranks/scripts/brightdata-serp-probe.py

  CONCURRENCY=8 KEYWORDS='fivetran alternative,pizza' \\
    python3 plugins/data_source/google_serp_ranks/scripts/brightdata-serp-probe.py
"""
from __future__ import annotations

import concurrent.futures
import json
import os
import sys
import time
import urllib.parse
import urllib.request
from urllib.error import HTTPError


def load_secret() -> tuple[str, str]:
    import subprocess

    raw = subprocess.check_output(
        [
            "aws",
            "secretsmanager",
            "get-secret-value",
            "--secret-id",
            "upfoundry/brightdata",
            "--query",
            "SecretString",
            "--output",
            "text",
        ],
        text=True,
    )
    secret = json.loads(raw)
    api_key = secret.get("BRIGHTDATA_API_KEY") or secret.get("api_key")
    zone = secret.get("BRIGHTDATA_ZONE") or secret.get("zone") or "serp_api1"
    if not api_key:
        raise SystemExit("BRIGHTDATA_API_KEY missing from upfoundry/brightdata secret")
    return api_key, zone


def is_soft_error(body: str) -> bool:
    trimmed = body.strip()
    if not trimmed:
        return True
    lower = trimmed.lower()
    if lower.startswith("{") or lower.startswith("["):
        return False
    return (
        "error while processing request" in lower
        or "unexpected error" in lower
        or lower == "error"
    )


def request_one(api_key: str, zone: str, keyword: str, start: int) -> dict:
    search_url = (
        "https://www.google.com/search?"
        + urllib.parse.urlencode(
            {
                "q": keyword,
                "hl": "en",
                "gl": "us",
                "start": str(start),
                "num": "10",
                "brd_json": "1",
            }
        )
    )
    payload = {
        "zone": zone,
        "url": search_url,
        "format": "raw",
        "data_format": "parsed_light",
    }
    req = urllib.request.Request(
        "https://api.brightdata.com/request",
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            status = resp.status
            headers = {k.lower(): v for k, v in resp.headers.items()}
    except HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        status = e.code
        headers = {k.lower(): v for k, v in e.headers.items()} if e.headers else {}
    except Exception as e:  # noqa: BLE001
        return {
            "keyword": keyword,
            "start": start,
            "error": str(e),
            "elapsed": round(time.time() - t0, 2),
        }

    ok_json = False
    try:
        json.loads(raw)
        ok_json = True
    except Exception:
        pass

    return {
        "keyword": keyword,
        "start": start,
        "status": status,
        "len": len(raw),
        "ok_json": ok_json,
        "soft_error": is_soft_error(raw),
        "preview": raw[:120],
        "brd_err_code": headers.get("x-brd-err-code", ""),
        "brd_err_msg": headers.get("x-brd-err-msg") or headers.get("x-brd-error", ""),
        "elapsed": round(time.time() - t0, 2),
    }


def main() -> int:
    api_key = os.environ.get("BRIGHTDATA_API_KEY")
    zone = os.environ.get("BRIGHTDATA_ZONE", "serp_api1")
    if not api_key:
        api_key, zone = load_secret()

    keywords = [
        k.strip()
        for k in os.environ.get(
            "KEYWORDS",
            "fivetran alternative,change data capture sql server,"
            "sql server to snowflake migration,migrate mssql to snowflake,"
            "airbyte vs fivetran,pizza",
        ).split(",")
        if k.strip()
    ]
    pages = [int(x) for x in os.environ.get("PAGES", "0,10,20").split(",") if x.strip()]
    concurrency = int(os.environ.get("CONCURRENCY", "8"))

    jobs = [(kw, start) for kw in keywords for start in pages]
    print(f"zone={zone} jobs={len(jobs)} concurrency={concurrency}", flush=True)

    results: list[dict] = []
    t0 = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as ex:
        futs = [ex.submit(request_one, api_key, zone, kw, st) for kw, st in jobs]
        for fut in concurrent.futures.as_completed(futs):
            row = fut.result()
            results.append(row)
            if row.get("soft_error") or not row.get("ok_json"):
                print("SOFT/FAIL", json.dumps(row), flush=True)

    soft = [r for r in results if r.get("soft_error")]
    hard = [r for r in results if not r.get("ok_json") and not r.get("soft_error")]
    ok = [r for r in results if r.get("ok_json")]
    print(
        f"done in {time.time() - t0:.1f}s ok={len(ok)} soft_error={len(soft)} other_fail={len(hard)}",
        flush=True,
    )
    for r in soft + hard:
        print(json.dumps(r), flush=True)
    return 1 if soft or hard else 0


if __name__ == "__main__":
    sys.exit(main())
