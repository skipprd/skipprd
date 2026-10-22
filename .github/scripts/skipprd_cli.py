"""Shared skipprd CLI argv. Host logging is `--log <level>`, not SKIPPR_LOG_LEVEL alone."""

from __future__ import annotations

import os


def log_args(env: dict[str, str] | None = None) -> list[str]:
    source = env if env is not None else os.environ
    level = (source.get("SKIPPR_LOG_LEVEL") or "info").strip() or "info"
    return ["--log", level]
