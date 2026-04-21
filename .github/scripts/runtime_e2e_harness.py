#!/usr/bin/env python3

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from runtime_plugin_catalog import (
    load_workspace_plugin_catalog,
    manifest_payload_for_catalog_entry,
    versioned_manifest_relative_path,
    workspace_runtime_protocol_version,
)
from runtime_plugin_targets import published_runtime_plugin_targets, resolve_target_artifact


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_AWS_REGION = "us-east-1"
DEFAULT_ASSERTION_OUTPUT = (
    "s3://skippr-e2e-sample-data-output/runtime-e2e-assertions"
)
EXPECTED_RUNTIME_PLUGIN_PATTERNS = (
    "skippr-plugin-data-source-s3*",
    "skippr-plugin-data-sink-athena*",
    "skippr-plugin-schema-sink-glue*",
)
LOCAL_SCENARIO_RUNTIME_MANIFESTS = (
    ("runtime_s3_source", "s3-source.json"),
    ("runtime_athena_sink", "athena-sink.json"),
    ("runtime_glue_schema", "glue-schema.json"),
)
LOCAL_SCENARIO_RUNTIME_PIPELINE_ANCHORS = {
    "bike_hire": "    data_sink: data_sinks.test_datalake\n",
    "bike_hire_many": "    data_sink: data_sinks.test_datalake\n",
    "bike_hire_s3_wal_many": "    data_sink: data_sinks.test_datalake\n",
    "deadletters_test": "    data_sink: data_sinks.test_datalake\n",
}
BIKE_HIRE_RUNTIME_VERSION_ANCHORS = (
    ("  s3_bike_hire:\n    S3:\n", "S3"),
    ("  test_datalake:\n    Athena:\n", "Athena"),
    ("  glue_bikehire:\n    Glue:\n", "Glue"),
)
DEADLETTERS_RUNTIME_VERSION_ANCHORS = (
    ("  s3_deadletters_test:\n    S3:\n", "S3"),
    ("  test_datalake:\n    Athena:\n", "Athena"),
    ("  test_deadletters:\n    Athena:\n", "Athena"),
    ("  glue_bikehire:\n    Glue:\n", "Glue"),
    ("  glue_deadletters:\n    Glue:\n", "Glue"),
)
SCENARIO_RUNTIME_VERSION_ANCHORS = {
    "bike_hire": BIKE_HIRE_RUNTIME_VERSION_ANCHORS,
    "bike_hire_many": BIKE_HIRE_RUNTIME_VERSION_ANCHORS,
    "bike_hire_s3_wal_many": BIKE_HIRE_RUNTIME_VERSION_ANCHORS,
    "deadletters_test": DEADLETTERS_RUNTIME_VERSION_ANCHORS,
}
RUNTIME_PLUGIN_SMOKE_SUPPORT_MANIFESTS = (
    "file-sink.json",
    "postgres-sink.json",
    "postgres-schema.json",
)
RUNTIME_PLUGIN_SMOKE_DOWNLOAD_MANIFESTS = {
    "smoke": frozenset(
        {
            "file-source.json",
            "file-sink.json",
        }
    ),
    "full": frozenset(
        {
            "file-source.json",
            "file-sink.json",
            "postgres-source.json",
            "postgres-sink.json",
            "postgres-schema.json",
        }
    ),
}
RUNTIME_FILE_SMOKE_PLUGIN_PATTERNS = (
    "skippr-plugin-data-source-file*",
    "skippr-plugin-data-sink-file*",
)
RUNTIME_POSTGRES_SMOKE_PLUGIN_PATTERNS = (
    "skippr-plugin-data-source-postgres*",
    "skippr-plugin-data-sink-postgres*",
    "skippr-plugin-schema-sink-postgres*",
)
RUNTIME_ACCEPTANCE_WORKSPACE = "runtime-plugin-acceptance"
RUNTIME_ACCEPTANCE_POSTGRES_SERVICES = ("postgres", "postgres-target")
DYNAMODB_TABLE = "Test-MetadataService-Stack-MetadataTable8CB34826-1OBKKG0QKVLJC"

SODA_INSTALLED = False
SODA_VENV_DIR: Path | None = None
METADATA_REFRESH_QUERY_PARAM = "skippr_metadata_refresh"


class HarnessError(RuntimeError):
    pass


@dataclass(frozen=True)
class SyncRun:
    pipeline: str
    extra_env: tuple[tuple[str, str], ...] = ()
    allow_exit_codes: tuple[int, ...] = ()


@dataclass(frozen=True)
class Scenario:
    name: str
    config_path: Path
    smoke_runs: tuple[SyncRun, ...]
    full_runs: tuple[SyncRun, ...]
    smoke_verifiers: tuple[str, ...] = ()
    full_verifiers: tuple[str, ...] = ()


@dataclass
class ScenarioContext:
    scenario: Scenario
    skippr_el: Path
    runtime_plugin_dir: Path
    base_env: dict[str, str]
    assertion_output: str


@dataclass(frozen=True)
class DownloadedRuntimeManifest:
    path: Path
    payload: dict


@dataclass(frozen=True)
class LocalStagedRuntimeManifests:
    manifest_dir: Path
    manifest_paths: dict[str, Path]
    manifest_versions: dict[str, str]


def scenario_config(relative_path: str) -> Path:
    return REPO_ROOT / relative_path


BIKE_HIRE_BASE_ENV = (
    ("STATS_HISTOGRAM_ENABLED", "true"),
    ("SKIPPR_DEBUG_LOGS", "true"),
    ("RUST_BACKTRACE", "1"),
)

BIKE_HIRE_CHAOS_ENV = BIKE_HIRE_BASE_ENV + (
    ("SKIPPR_CHAOS_MODE", "yes"),
    ("SKIPPR_CHAOS_MIN_SECONDS", "15"),
    ("SKIPPR_CHAOS_MAX_SECONDS", "25"),
)

BIKE_HIRE_STEADY_ENV = BIKE_HIRE_BASE_ENV + (("SKIPPR_CHAOS_MODE", "no"),)

BIKE_HIRE_MANY_CHAOS_ENV = (
    ("RUST_BACKTRACE", "1"),
    ("SKIPPR_CHAOS_MODE", "yes"),
    ("SKIPPR_CHAOS_MIN_SECONDS", "20"),
    ("SKIPPR_CHAOS_MAX_SECONDS", "30"),
)


SCENARIOS = {
    "bike_hire": Scenario(
        name="bike_hire",
        config_path=scenario_config(".github/actions/e2e/bike_hire/skippr-el.yml"),
        smoke_runs=(
            SyncRun(
                pipeline="bike_hire",
                extra_env=BIKE_HIRE_STEADY_ENV,
            ),
        ),
        full_runs=(
            SyncRun(
                pipeline="bike_hire",
                extra_env=BIKE_HIRE_CHAOS_ENV,
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire",
                extra_env=BIKE_HIRE_CHAOS_ENV,
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire",
                extra_env=BIKE_HIRE_CHAOS_ENV,
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire",
                extra_env=BIKE_HIRE_CHAOS_ENV,
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire",
                extra_env=BIKE_HIRE_STEADY_ENV,
            ),
        ),
        smoke_verifiers=("bike_hire_rows",),
        full_verifiers=("soda_bike_hire",),
    ),
    "bike_hire_many": Scenario(
        name="bike_hire_many",
        config_path=scenario_config(".github/actions/e2e/bike_hire_many/skippr-el.yml"),
        smoke_runs=(
            SyncRun(
                pipeline="bike_hire_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("SKIPPR_CHAOS_MODE", "no"),
                ),
            ),
        ),
        full_runs=(
            SyncRun(
                pipeline="bike_hire_many",
                extra_env=BIKE_HIRE_MANY_CHAOS_ENV,
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_many",
                extra_env=BIKE_HIRE_MANY_CHAOS_ENV,
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_many",
                extra_env=BIKE_HIRE_MANY_CHAOS_ENV,
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_many",
                extra_env=BIKE_HIRE_MANY_CHAOS_ENV,
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("SKIPPR_CHAOS_MODE", "no"),
                ),
            ),
        ),
        smoke_verifiers=("bike_hire_many_rows",),
        full_verifiers=("bike_hire_many_pruning", "soda_bike_hire_many"),
    ),
    "bike_hire_s3_wal_many": Scenario(
        name="bike_hire_s3_wal_many",
        config_path=scenario_config(
            ".github/actions/e2e/bike_hire_s3_wal_many/skippr-el.yml"
        ),
        smoke_runs=(
            SyncRun(
                pipeline="bike_hire_s3_wal_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("WAL_STORAGE", "s3"),
                    ("SKIPPR_CHAOS_MODE", "no"),
                ),
            ),
        ),
        full_runs=(
            SyncRun(
                pipeline="bike_hire_s3_wal_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("WAL_STORAGE", "s3"),
                    ("SKIPPR_CHAOS_MODE", "yes"),
                ),
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_s3_wal_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("WAL_STORAGE", "s3"),
                    ("SKIPPR_CHAOS_MODE", "yes"),
                ),
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_s3_wal_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("WAL_STORAGE", "s3"),
                    ("SKIPPR_CHAOS_MODE", "yes"),
                ),
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_s3_wal_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("WAL_STORAGE", "s3"),
                    ("SKIPPR_CHAOS_MODE", "yes"),
                ),
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_s3_wal_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("WAL_STORAGE", "s3"),
                    ("SKIPPR_CHAOS_MODE", "yes"),
                ),
                allow_exit_codes=(137,),
            ),
            SyncRun(
                pipeline="bike_hire_s3_wal_many",
                extra_env=(
                    ("RUST_BACKTRACE", "1"),
                    ("WAL_STORAGE", "s3"),
                    ("SKIPPR_CHAOS_MODE", "no"),
                ),
            ),
        ),
        smoke_verifiers=("bike_hire_s3_wal_many_rows",),
        full_verifiers=("soda_bike_hire_s3_wal_many",),
    ),
    "deadletters_test": Scenario(
        name="deadletters_test",
        config_path=scenario_config(".github/actions/e2e/deadletters/skippr-el.yml"),
        smoke_runs=(SyncRun(pipeline="deadletters_test"),),
        full_runs=(SyncRun(pipeline="deadletters_test"),),
        smoke_verifiers=("deadletters_athena_routing",),
        full_verifiers=("soda_deadletters",),
    ),
}


def print_step(message: str) -> None:
    print(f"[runtime-e2e] {message}", flush=True)


def ensure_tool(name: str) -> str:
    path = shutil.which(name)
    if not path:
        raise HarnessError(f"required tool '{name}' was not found on PATH")
    return path


def run_command(
    command: list[str],
    *,
    env: dict[str, str] | None = None,
    cwd: Path = REPO_ROOT,
    allow_exit_codes: tuple[int, ...] = (),
    capture_output: bool = False,
) -> subprocess.CompletedProcess[str]:
    print_step(f"Running: {' '.join(command)}")
    completed = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        text=True,
        capture_output=capture_output,
        check=False,
    )
    normalized_returncode = (
        128 + abs(completed.returncode)
        if completed.returncode < 0
        else completed.returncode
    )
    allowed = {0, *allow_exit_codes}
    if completed.returncode not in allowed and normalized_returncode not in allowed:
        if capture_output:
            if completed.stdout:
                print(completed.stdout, end="", file=sys.stdout)
            if completed.stderr:
                print(completed.stderr, end="", file=sys.stderr)
        raise HarnessError(
            "command failed with exit code "
            f"{completed.returncode} (normalized={normalized_returncode}): "
            f"{' '.join(command)}"
        )
    if completed.returncode in allow_exit_codes or normalized_returncode in allow_exit_codes:
        print_step(
            "Allowed non-zero exit observed "
            f"({completed.returncode}, normalized={normalized_returncode}): "
            f"{' '.join(command)}"
        )
    return completed


def capture_text(command: list[str], *, env: dict[str, str] | None = None) -> str:
    completed = run_command(command, env=env, capture_output=True)
    return completed.stdout.strip()


def capture_json(command: list[str], *, env: dict[str, str] | None = None) -> dict:
    stdout = capture_text(command, env=env)
    return json.loads(stdout) if stdout else {}


def resolve_skippr_el(path_arg: str | None) -> Path:
    candidates: list[Path] = []
    if path_arg:
        candidates.append(Path(path_arg))
    env_path = os.environ.get("SKIPPR_E2E_SKIPPR_EL_BIN")
    if env_path:
        candidates.append(Path(env_path))
    candidates.extend(
        [
            REPO_ROOT / "skippr-el-linux_x86" / "skippr-el",
            REPO_ROOT / "skippr-el-macos_arm64" / "skippr-el",
            REPO_ROOT / "skippr-el-windows_x86" / "skippr-el.exe",
            REPO_ROOT / "target" / "release" / "skippr-el",
            REPO_ROOT / "target" / "debug" / "skippr-el",
        ]
    )

    for candidate in candidates:
        candidate = candidate.expanduser().resolve()
        if candidate.is_dir():
            nested = candidate / "skippr-el"
            if nested.exists():
                candidate = nested
            else:
                nested_exe = candidate / "skippr-el.exe"
                if nested_exe.exists():
                    candidate = nested_exe
        if candidate.is_file():
            candidate.chmod(candidate.stat().st_mode | 0o111)
            return candidate

    searched = "\n".join(f"  - {candidate}" for candidate in candidates)
    raise HarnessError(f"could not find skippr-el binary; searched:\n{searched}")


def assert_no_bundled_runtime_plugins(skippr_el: Path) -> None:
    artifact_dir = skippr_el.parent
    bundled = sorted(artifact_dir.glob("skippr-plugin-*"))
    if bundled:
        joined = ", ".join(path.name for path in bundled)
        raise HarnessError(
            "skippr-el artifact unexpectedly contains runtime plugin binaries: "
            f"{joined}"
        )


def find_runtime_plugins_matching(
    runtime_plugin_dir: Path, patterns: tuple[str, ...]
) -> list[Path]:
    matches: list[Path] = []
    for path in runtime_plugin_dir.rglob("*"):
        if not path.is_file():
            continue
        if any(fnmatch.fnmatch(path.name, pattern) for pattern in patterns):
            matches.append(path)
    return sorted(matches)


def find_downloaded_plugins(runtime_plugin_dir: Path) -> list[Path]:
    return find_runtime_plugins_matching(runtime_plugin_dir, EXPECTED_RUNTIME_PLUGIN_PATTERNS)


def assert_runtime_plugins_downloaded(runtime_plugin_dir: Path) -> None:
    matches = find_downloaded_plugins(runtime_plugin_dir)
    if not matches:
        raise HarnessError(
            "expected runtime plugins to be downloaded into "
            f"{runtime_plugin_dir}"
        )
    print_step("Downloaded runtime plugins:")
    for path in matches:
        print(path)


def assert_runtime_plugin_patterns_downloaded(
    runtime_plugin_dir: Path, patterns: tuple[str, ...]
) -> None:
    matches = find_runtime_plugins_matching(runtime_plugin_dir, patterns)
    if not matches:
        joined = ", ".join(patterns)
        raise HarnessError(
            "expected runtime plugins matching "
            f"{joined} to be downloaded into {runtime_plugin_dir}"
        )
    print_step("Downloaded runtime plugin artifacts:")
    for path in matches:
        print(path)


def base_environment(
    *,
    skippr_el: Path,
    config_path: Path,
    runtime_plugin_dir: Path,
    extra_env: dict[str, str] | None = None,
) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("AWS_DEFAULT_REGION", DEFAULT_AWS_REGION)
    env["SKIPPR_CONFIG_FILE"] = str(config_path)
    env["SKIPPR_RUNTIME_PLUGIN_DIR"] = str(runtime_plugin_dir)
    env["SKIPPR_RUNTIME_LOG_LEVEL"] = env.get("SKIPPR_RUNTIME_LOG_LEVEL", "info")
    env["SKIPPR_E2E_SKIPPR_EL_BIN"] = str(skippr_el)
    if extra_env:
        env.update(extra_env)
    return env


def run_sync(skippr_el: Path, sync_run: SyncRun, base_env: dict[str, str]) -> None:
    env = base_env.copy()
    for key, value in sync_run.extra_env:
        env[key] = value
    run_command(
        [
            str(skippr_el),
            "sync",
            "--log",
            "--pipeline",
            sync_run.pipeline,
        ],
        env=env,
        allow_exit_codes=sync_run.allow_exit_codes,
    )


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fresh_metadata_url(url: str) -> str:
    parsed = urllib.parse.urlsplit(url)
    query = urllib.parse.parse_qsl(parsed.query, keep_blank_values=True)
    query.append((METADATA_REFRESH_QUERY_PARAM, str(time.time_ns())))
    return urllib.parse.urlunsplit(
        parsed._replace(query=urllib.parse.urlencode(query))
    )


def metadata_request(url: str, *, method: str | None = None) -> urllib.request.Request:
    return urllib.request.Request(
        fresh_metadata_url(url),
        method=method,
        headers={
            "Cache-Control": "no-cache, no-store, max-age=0",
            "Pragma": "no-cache",
        },
    )


def download_url(
    url: str,
    destination: Path,
    *,
    executable: bool = False,
    fresh_metadata: bool = False,
) -> None:
    print_step(f"Downloading {url}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    try:
        request = metadata_request(url) if fresh_metadata else url
        with urllib.request.urlopen(request) as response:
            with destination.open("wb") as handle:
                shutil.copyfileobj(response, handle)
    except urllib.error.HTTPError as err:
        raise HarnessError(f"download failed for {url}: HTTP {err.code}") from err
    except urllib.error.URLError as err:
        raise HarnessError(f"download failed for {url}: {err}") from err
    if executable:
        destination.chmod(destination.stat().st_mode | 0o111)


def assert_remote_url_exists(url: str) -> None:
    print_step(f"Verifying {url}")
    request = urllib.request.Request(url, method="HEAD")
    try:
        with urllib.request.urlopen(request):
            return
    except urllib.error.HTTPError as err:
        if err.code == 405:
            fallback_request = urllib.request.Request(
                url,
                headers={"Range": "bytes=0-0"},
            )
            try:
                with urllib.request.urlopen(fallback_request):
                    return
            except urllib.error.HTTPError as fallback_err:
                raise HarnessError(
                    f"artifact verification failed for {url}: HTTP {fallback_err.code}"
                ) from fallback_err
            except urllib.error.URLError as fallback_err:
                raise HarnessError(
                    f"artifact verification failed for {url}: {fallback_err}"
                ) from fallback_err
        raise HarnessError(f"artifact verification failed for {url}: HTTP {err.code}") from err
    except urllib.error.URLError as err:
        raise HarnessError(f"artifact verification failed for {url}: {err}") from err


def published_target_for_architecture_name(architecture_name: str):
    expected_dir = f"runtime-plugin-binaries-{architecture_name}"
    for target in published_runtime_plugin_targets(REPO_ROOT):
        if target.publish_artifact_dir == expected_dir:
            return target
    raise HarnessError(
        "unsupported runtime plugin architecture name "
        f"{architecture_name!r}; expected one of "
        f"{', '.join(sorted(target.publish_artifact_dir.removeprefix('runtime-plugin-binaries-') for target in published_runtime_plugin_targets(REPO_ROOT)))}"
    )


def runtime_source_manifest_filenames(catalog_entries: list[dict]) -> list[str]:
    filenames = sorted(
        {
            entry["manifest_filename"]
            for entry in catalog_entries
            if entry["manifest_kind"] == "DataSource"
        }
    )
    if not filenames:
        raise HarnessError("runtime plugin catalog returned no source manifests")
    return filenames


def runtime_release_manifest_filenames(catalog_entries: list[dict]) -> list[str]:
    available = {entry["manifest_filename"] for entry in catalog_entries}
    missing_support = sorted(
        manifest
        for manifest in RUNTIME_PLUGIN_SMOKE_SUPPORT_MANIFESTS
        if manifest not in available
    )
    if missing_support:
        raise HarnessError(
            "runtime plugin catalog is missing required smoke manifests: "
            + ", ".join(missing_support)
        )
    return sorted(
        set(runtime_source_manifest_filenames(catalog_entries))
        | set(RUNTIME_PLUGIN_SMOKE_SUPPORT_MANIFESTS)
    )


def runtime_full_download_manifests(mode: str) -> frozenset[str]:
    return RUNTIME_PLUGIN_SMOKE_DOWNLOAD_MANIFESTS[mode]


def parse_runtime_plugin_versions(
    values: list[str] | None,
) -> dict[str, str] | None:
    if not values:
        return None

    parsed: dict[str, str] = {}
    for value in values:
        plugin_name, separator, version = value.partition("=")
        plugin_name = plugin_name.strip()
        version = version.strip()
        if not separator or not plugin_name or not version:
            raise HarnessError(
                "--runtime-plugin-version must use the form PLUGIN=VERSION"
            )
        previous = parsed.get(plugin_name)
        if previous is not None and previous != version:
            raise HarnessError(
                f"runtime plugin {plugin_name!r} was pinned more than once with conflicting versions"
            )
        parsed[plugin_name] = version
    return parsed or None


def public_release_base_url(releases_bucket: str, runtime_plugin_release_subdir: str) -> str:
    _ = releases_bucket
    return f"https://install.skippr.io/releases/{runtime_plugin_release_subdir.strip('/')}"


def published_latest_manifest_index_url(
    releases_bucket: str,
    runtime_plugin_release_subdir: str,
) -> str:
    return (
        f"{public_release_base_url(releases_bucket, runtime_plugin_release_subdir)}"
        "/latest/manifest-index.json"
    )


def versioned_manifest_relative_path_for(entry: dict, version: str) -> Path:
    return (
        Path("plugins")
        / entry["manifest_stem"]
        / "versions"
        / version
        / entry["manifest_filename"]
    )


def published_versioned_manifest_url(
    *,
    releases_bucket: str,
    runtime_plugin_release_subdir: str,
    entry: dict,
    version: str,
) -> str:
    relative_path = versioned_manifest_relative_path_for(entry, version)
    return (
        f"{public_release_base_url(releases_bucket, runtime_plugin_release_subdir)}"
        f"/{relative_path.as_posix()}"
    )


def fetch_json_url(url: str) -> dict:
    try:
        with urllib.request.urlopen(metadata_request(url)) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as err:
        raise HarnessError(f"failed to fetch JSON from {url}: HTTP {err.code}") from err
    except urllib.error.URLError as err:
        raise HarnessError(f"failed to fetch JSON from {url}: {err}") from err
    except json.JSONDecodeError as err:
        raise HarnessError(f"failed to parse JSON from {url}: {err}") from err


def download_runtime_release_manifests(
    manifest_urls: dict[str, str],
    destination_dir: Path,
) -> dict[str, DownloadedRuntimeManifest]:
    manifests: dict[str, DownloadedRuntimeManifest] = {}
    for manifest_filename, manifest_url in manifest_urls.items():
        manifest_path = destination_dir / manifest_filename
        download_url(manifest_url, manifest_path, fresh_metadata=True)
        payload = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifests[manifest_filename] = DownloadedRuntimeManifest(
            path=manifest_path,
            payload=payload,
        )
    return manifests


def verify_runtime_release_artifacts(
    manifests: dict[str, DownloadedRuntimeManifest],
    *,
    source_manifests: set[str],
    full_download_manifests: set[str],
    catalog_by_manifest: dict[str, dict],
    expected_versions_by_manifest: dict[str, str] | None,
    expected_protocol_version: int,
    target,
    download_dir: Path,
) -> None:
    for manifest_filename, downloaded in manifests.items():
        expected_metadata = catalog_by_manifest.get(manifest_filename)
        if expected_metadata is None:
            raise HarnessError(
                f"runtime plugin catalog is missing metadata for {manifest_filename}"
            )
        expected_version = (
            expected_versions_by_manifest.get(manifest_filename)
            if expected_versions_by_manifest is not None
            else expected_metadata["package_version"]
        )
        if expected_version is None:
            expected_version = expected_metadata["package_version"]
        actual_version = downloaded.payload.get("version")
        if actual_version != expected_version:
            raise HarnessError(
                f"manifest {manifest_filename} declares version {actual_version!r}, "
                f"expected {expected_version!r}"
            )
        actual_protocol = downloaded.payload.get("protocol_version")
        if actual_protocol != expected_protocol_version:
            raise HarnessError(
                f"manifest {manifest_filename} declares protocol version {actual_protocol!r}, "
                f"expected {expected_protocol_version!r}"
            )
        artifact = resolve_target_artifact(downloaded.payload.get("artifacts", {}), target)
        if not artifact:
            if manifest_filename in source_manifests or manifest_filename in full_download_manifests:
                raise HarnessError(
                    f"manifest {manifest_filename} has no artifact for target {target.triple}"
                )
            continue
        artifact_url = artifact.get("url")
        if not artifact_url:
            raise HarnessError(
                f"manifest {manifest_filename} is missing an artifact URL for target {target.triple}"
            )
        if manifest_filename in full_download_manifests:
            executable_name = Path(artifact.get("executable", "")).name
            if not executable_name:
                raise HarnessError(
                    f"manifest {manifest_filename} is missing an executable name for target {target.triple}"
                )
            artifact_path = download_dir / manifest_filename.removesuffix(".json") / executable_name
            download_url(artifact_url, artifact_path, executable=True)
            expected_sha256 = artifact.get("sha256")
            if expected_sha256:
                actual_sha256 = sha256(artifact_path)
                if actual_sha256 != expected_sha256:
                    raise HarnessError(
                        "runtime artifact checksum mismatch for "
                        f"{manifest_filename}: expected {expected_sha256} got {actual_sha256}"
                    )
            continue
        if manifest_filename in source_manifests:
            assert_remote_url_exists(artifact_url)


def runtime_acceptance_test_env(skippr_el: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["SKIPPR_E2E_SKIPPR_EL_BIN"] = str(skippr_el)
    env["CARGO_BUILD_JOBS"] = env.get("CARGO_BUILD_JOBS", "2")
    return env


def skippr_el_build_profile(skippr_el: Path) -> str:
    return "debug" if skippr_el.parent.name == "debug" else "release"


def current_rust_target_triple() -> str:
    for line in capture_text([ensure_tool("rustc"), "-vV"]).splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    raise HarnessError("failed to determine current Rust host target triple")


def target_binary_name(binary_name: str, target_triple: str) -> str:
    if "windows" in target_triple and not binary_name.endswith(".exe"):
        return f"{binary_name}.exe"
    return binary_name


def stage_local_runtime_release(
    *,
    skippr_el: Path,
    output_dir: Path,
) -> LocalStagedRuntimeManifests:
    ensure_tool("cargo")
    catalog_entries = load_workspace_plugin_catalog(REPO_ROOT)
    catalog_by_manifest = {
        entry["manifest_filename"]: entry for entry in catalog_entries
    }
    protocol_version = workspace_runtime_protocol_version(REPO_ROOT)
    target_triple = current_rust_target_triple()
    profile = skippr_el_build_profile(skippr_el)
    build_target_dir = output_dir.expanduser().resolve() / "build-target"

    packages_to_build: list[str] = []
    for _, manifest_filename in LOCAL_SCENARIO_RUNTIME_MANIFESTS:
        metadata = catalog_by_manifest.get(manifest_filename)
        if metadata is None:
            raise HarnessError(
                f"runtime plugin catalog is missing required local manifest {manifest_filename}"
            )
        packages_to_build.append(metadata["package_name"])

    build_command = [ensure_tool("cargo"), "build"]
    if profile == "release":
        build_command.append("--release")
    for package_name in packages_to_build:
        build_command.extend(["-p", package_name])
    build_env = os.environ.copy()
    build_env["CARGO_TARGET_DIR"] = str(build_target_dir)
    run_command(build_command, env=build_env)

    output_dir = output_dir.expanduser().resolve()
    manifest_index_entries = []
    manifest_paths: dict[str, Path] = {}
    manifest_versions: dict[str, str] = {}

    for config_key, manifest_filename in LOCAL_SCENARIO_RUNTIME_MANIFESTS:
        metadata = catalog_by_manifest[manifest_filename]
        binary_filename = target_binary_name(metadata["binary_name"], target_triple)
        built_binary = build_target_dir / profile / binary_filename
        if not built_binary.exists():
            raise HarnessError(
                f"expected built runtime plugin binary at {built_binary} for {manifest_filename}"
            )

        # Keep local manifests pointed at the build-target binary directly.
        # Relocating macOS debug binaries into a second tree can make the child
        # processes stop responding before the first runtime protocol frame.
        staged_binary = built_binary

        output_manifest_path = output_dir / versioned_manifest_relative_path(metadata)
        output_manifest_path.parent.mkdir(parents=True, exist_ok=True)
        relative_executable = Path(
            os.path.relpath(staged_binary, output_manifest_path.parent)
        ).as_posix()
        manifest_payload = manifest_payload_for_catalog_entry(
            metadata,
            protocol_version=protocol_version,
            artifacts={
                target_triple: {
                    "executable": relative_executable,
                    "sha256": sha256(staged_binary),
                }
            },
            executable=relative_executable,
            build_checksum=metadata["checksum"],
        )
        output_manifest_path.write_text(
            json.dumps(manifest_payload, indent=2) + "\n",
            encoding="utf-8",
        )
        manifest_paths[config_key] = output_manifest_path
        manifest_versions[config_key] = metadata["package_version"]
        manifest_index_entries.append(
            {
                "name": manifest_payload["name"],
                "plugin_name": manifest_payload["plugin_name"],
                "kind": manifest_payload["kind"],
                "manifest_filename": manifest_filename,
                "manifest_url": output_manifest_path.as_uri(),
            }
        )

    manifest_index = {
        "bundle_version": "local",
        "manifests": manifest_index_entries,
    }
    latest_index_path = output_dir / "latest" / "manifest-index.json"
    latest_index_path.parent.mkdir(parents=True, exist_ok=True)
    latest_index_path.write_text(
        json.dumps(manifest_index, indent=2) + "\n",
        encoding="utf-8",
    )

    print_step(f"Staged local runtime release manifests under {output_dir}")
    return LocalStagedRuntimeManifests(
        manifest_dir=output_dir,
        manifest_paths=manifest_paths,
        manifest_versions=manifest_versions,
    )


def load_local_runtime_manifests(
    manifest_dir: Path,
) -> LocalStagedRuntimeManifests:
    manifest_dir = manifest_dir.expanduser().resolve()
    catalog_by_manifest = {
        entry["manifest_filename"]: entry for entry in load_workspace_plugin_catalog(REPO_ROOT)
    }
    manifest_paths: dict[str, Path] = {}
    manifest_versions: dict[str, str] = {}
    for config_key, manifest_filename in LOCAL_SCENARIO_RUNTIME_MANIFESTS:
        metadata = catalog_by_manifest.get(manifest_filename)
        if metadata is None:
            raise HarnessError(
                f"runtime plugin catalog is missing required local manifest {manifest_filename}"
            )
        manifest_path = manifest_dir / versioned_manifest_relative_path(metadata)
        if not manifest_path.exists():
            raise HarnessError(
                f"expected local staged runtime manifest at {manifest_path}"
            )
        manifest_paths[config_key] = manifest_path
        manifest_versions[config_key] = metadata["package_version"]
    return LocalStagedRuntimeManifests(
        manifest_dir=manifest_dir,
        manifest_paths=manifest_paths,
        manifest_versions=manifest_versions,
    )


def runtime_plugin_version_config_text(
    scenario_name: str,
    config_text: str,
    runtime_plugin_versions: dict[str, str] | None,
) -> str:
    if not runtime_plugin_versions:
        return config_text

    anchors = SCENARIO_RUNTIME_VERSION_ANCHORS.get(scenario_name, ())
    if not anchors:
        return config_text

    rewritten = config_text
    for anchor, plugin_name in anchors:
        version = runtime_plugin_versions.get(plugin_name)
        if version is None:
            continue
        normalized = version.strip()
        if not normalized:
            continue
        count = rewritten.count(anchor)
        if count != 1:
            raise HarnessError(
                f"expected to find anchor {anchor!r} exactly once in scenario {scenario_name}, found {count}"
            )
        print_step(
            f"Pinning scenario {scenario_name} plugin {plugin_name} to version {normalized}"
        )
        rewritten = rewritten.replace(
            anchor,
            anchor + f'      version: "{normalized}"\n',
            1,
        )
    return rewritten


def local_runtime_config_text(
    scenario_name: str,
    config_text: str,
    local_runtime_manifests: LocalStagedRuntimeManifests | None,
) -> str:
    if local_runtime_manifests is None:
        return config_text

    anchor = LOCAL_SCENARIO_RUNTIME_PIPELINE_ANCHORS.get(scenario_name)
    if anchor is None:
        return config_text
    if "\nruntime_plugins:\n" in config_text:
        raise HarnessError(
            f"scenario {scenario_name} already declares runtime_plugins; local staged runtime injection is unsupported"
        )

    count = config_text.count(anchor)
    if count != 1:
        raise HarnessError(
            f"expected to find anchor {anchor!r} exactly once in scenario {scenario_name}, found {count}"
        )

    runtime_lines = (
        "    runtime_input: runtime_plugins.runtime_s3_source\n"
        "    runtime_output: runtime_plugins.runtime_athena_sink\n"
        "    runtime_schema: runtime_plugins.runtime_glue_schema\n"
    )
    rewritten = config_text.replace(anchor, anchor + runtime_lines, 1)
    if not rewritten.endswith("\n"):
        rewritten += "\n"
    runtime_plugins_block = (
        "\nruntime_plugins:\n"
        f'  runtime_s3_source:\n    manifest: "{local_runtime_manifests.manifest_paths["runtime_s3_source"]}"\n'
        f'  runtime_athena_sink:\n    manifest: "{local_runtime_manifests.manifest_paths["runtime_athena_sink"]}"\n'
        f'  runtime_glue_schema:\n    manifest: "{local_runtime_manifests.manifest_paths["runtime_glue_schema"]}"\n'
    )
    print_step(
        f"Using local staged runtime manifests for {scenario_name} from {local_runtime_manifests.manifest_dir}"
    )
    return rewritten + runtime_plugins_block


def materialize_scenario_config(
    scenario: Scenario,
    runtime_plugin_dir: Path,
    runtime_plugin_versions: dict[str, str] | None,
    local_runtime_manifests: LocalStagedRuntimeManifests | None,
) -> Path:
    if (
        not runtime_plugin_versions
        and local_runtime_manifests is None
    ):
        return scenario.config_path

    config_text = scenario.config_path.read_text(encoding="utf-8")
    rewritten = runtime_plugin_version_config_text(
        scenario.name,
        config_text,
        runtime_plugin_versions,
    )
    rewritten = local_runtime_config_text(
        scenario.name,
        rewritten,
        local_runtime_manifests,
    )
    suffix_parts = [scenario.name]
    if runtime_plugin_versions:
        suffix_parts.append("pinned-runtime-versions")
    if local_runtime_manifests is not None:
        suffix_parts.append("local-staged-runtime")
    config_path = runtime_plugin_dir / ("-".join(suffix_parts) + ".yml")
    config_path.write_text(rewritten, encoding="utf-8")
    return config_path


def run_runtime_file_release_smoke(
    *,
    skippr_el: Path,
    runtime_plugin_dir: Path,
    manifests: dict[str, DownloadedRuntimeManifest],
) -> None:
    data_dir = Path(tempfile.mkdtemp(prefix="skippr_runtime_file_release_smoke_"))
    cleanup = True
    try:
        pipeline_name = "runtime_release_file_smoke"
        config_path = data_dir / "skippr-el.yml"
        config_path.write_text(
            f"""skippr:
  workspace: {RUNTIME_ACCEPTANCE_WORKSPACE}
  storage_mode: local

data_sources:
  file_source:
    File:
      path: "{REPO_ROOT / 'tests' / 'fixtures' / 'formats' / 'people.csv'}"
      format: csv

data_sinks:
  file_sink:
    File:
      format: parquet

runtime_plugins:
  file_runtime_source:
    manifest: "{manifests['file-source.json'].path}"
  file_runtime_sink:
    manifest: "{manifests['file-sink.json'].path}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.file_source
    data_sink: data_sinks.file_sink
    runtime_input: runtime_plugins.file_runtime_source
    runtime_output: runtime_plugins.file_runtime_sink
""",
            encoding="utf-8",
        )
        env = base_environment(
            skippr_el=skippr_el,
            config_path=config_path,
            runtime_plugin_dir=runtime_plugin_dir,
        )
        env["DATA_DIR"] = str(data_dir)
        env["DATA_DIR_MIN_FREE_BYTES"] = "0"
        run_command(
            [str(skippr_el), "sync", "--log", "--pipeline", pipeline_name],
            env=env,
        )
        parquet_files = sorted(data_dir.rglob("*.parquet"))
        if not parquet_files:
            raise HarnessError(
                "runtime file smoke completed but no parquet output was produced"
            )
        assert_runtime_plugin_patterns_downloaded(
            runtime_plugin_dir,
            RUNTIME_FILE_SMOKE_PLUGIN_PATTERNS,
        )
    except Exception:
        cleanup = False
        print_step(f"Preserving file smoke data dir for debugging: {data_dir}")
        raise
    finally:
        if cleanup:
            shutil.rmtree(data_dir, ignore_errors=True)


def wait_for_compose_service(service: str) -> None:
    container_id = capture_text(["docker", "compose", "ps", "-q", service])
    if not container_id:
        raise HarnessError(f"no docker compose container found for service {service!r}")
    for _ in range(30):
        status = capture_text(
            [
                "docker",
                "inspect",
                "-f",
                "{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}",
                container_id,
            ]
        )
        if status in {"healthy", "running"}:
            return
        time.sleep(2)
    raise HarnessError(f"service {service!r} did not become healthy in time")


def docker_compose_psql(service: str, sql: str, *, capture: bool = False) -> str:
    command = [
        "docker",
        "compose",
        "exec",
        "-T",
        service,
        "psql",
        "-v",
        "ON_ERROR_STOP=1",
        "-U",
        "postgres",
        "-d",
        "skippr_test",
        "-t",
        "-A",
        "-c",
        sql,
    ]
    if capture:
        return capture_text(command)
    run_command(command)
    return ""


def run_runtime_postgres_release_smoke(
    *,
    skippr_el: Path,
    runtime_plugin_dir: Path,
    manifests: dict[str, DownloadedRuntimeManifest],
) -> None:
    data_dir = Path(tempfile.mkdtemp(prefix="skippr_runtime_postgres_release_smoke_"))
    cleanup = True
    try:
        ensure_tool("docker")
        run_command(["docker", "compose", "up", "-d", *RUNTIME_ACCEPTANCE_POSTGRES_SERVICES])
        for service in RUNTIME_ACCEPTANCE_POSTGRES_SERVICES:
            wait_for_compose_service(service)

        docker_compose_psql(
            "postgres",
            """
DROP SCHEMA IF EXISTS public CASCADE;
CREATE SCHEMA public;
CREATE TABLE runtime_release_people (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);
INSERT INTO runtime_release_people (id, name)
VALUES (1, 'Ada'), (2, 'Grace');
""",
        )
        docker_compose_psql(
            "postgres-target",
            """
DROP SCHEMA IF EXISTS public CASCADE;
CREATE SCHEMA public;
""",
        )

        pipeline_name = "runtime_release_postgres_smoke"
        config_path = data_dir / "skippr-el.yml"
        config_path.write_text(
            f"""skippr:
  workspace: {RUNTIME_ACCEPTANCE_WORKSPACE}
  storage_mode: local

data_sources:
  postgres_source:
    Postgres:
      host: 127.0.0.1
      port: 15432
      user: postgres
      password: testpass
      database: skippr_test
      tables: ["runtime_release_people"]

data_sinks:
  postgres_sink:
    Postgres:
      host: 127.0.0.1
      port: 15433
      user: postgres
      password: testpass
      database: skippr_test
      schema_sink: schema_sinks.postgres_schema

schema_sinks:
  postgres_schema:
    Postgres:
      host: 127.0.0.1
      port: 15433
      user: postgres
      password: testpass
      database: skippr_test

runtime_plugins:
  postgres_runtime_source:
    manifest: "{manifests['postgres-source.json'].path}"
  postgres_runtime_sink:
    manifest: "{manifests['postgres-sink.json'].path}"
  postgres_runtime_schema:
    manifest: "{manifests['postgres-schema.json'].path}"

pipelines:
  {pipeline_name}:
    data_source: data_sources.postgres_source
    data_sink: data_sinks.postgres_sink
    runtime_input: runtime_plugins.postgres_runtime_source
    runtime_output: runtime_plugins.postgres_runtime_sink
    runtime_schema: runtime_plugins.postgres_runtime_schema
""",
            encoding="utf-8",
        )
        env = base_environment(
            skippr_el=skippr_el,
            config_path=config_path,
            runtime_plugin_dir=runtime_plugin_dir,
        )
        env["DATA_DIR"] = str(data_dir)
        env["DATA_DIR_MIN_FREE_BYTES"] = "0"
        run_command(
            [str(skippr_el), "sync", "--log", "--pipeline", pipeline_name],
            env=env,
        )
        table_names = [
            line.strip()
            for line in docker_compose_psql(
                "postgres-target",
                "SELECT table_name FROM information_schema.tables "
                "WHERE table_schema = 'public' ORDER BY table_name;",
                capture=True,
            ).splitlines()
            if line.strip()
        ]
        if not table_names:
            raise HarnessError(
                "runtime postgres smoke completed but no target tables were created"
            )
        row_counts = {
            table_name: int(
                docker_compose_psql(
                    "postgres-target",
                    f'SELECT COUNT(*) FROM "{table_name}";',
                    capture=True,
                ).strip()
            )
            for table_name in table_names
        }
        if max(row_counts.values()) < 2:
            raise HarnessError(
                "runtime postgres smoke completed but target row counts were unexpected: "
                + ", ".join(f"{name}={count}" for name, count in sorted(row_counts.items()))
            )
        assert_runtime_plugin_patterns_downloaded(
            runtime_plugin_dir,
            RUNTIME_POSTGRES_SMOKE_PLUGIN_PATTERNS,
        )
    except Exception:
        cleanup = False
        print_step(f"Preserving postgres smoke data dir for debugging: {data_dir}")
        raise
    finally:
        subprocess.run(
            ["docker", "compose", "down", "-v"],
            cwd=REPO_ROOT,
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if cleanup:
            shutil.rmtree(data_dir, ignore_errors=True)


def run_runtime_plugin_acceptance(
    *,
    skippr_el_arg: str | None,
    mode: str,
    architecture_name: str,
    releases_bucket: str,
    runtime_plugin_versions: dict[str, str] | None,
    runtime_plugin_release_subdir: str,
) -> None:
    ensure_tool("cargo")
    skippr_el = resolve_skippr_el(skippr_el_arg)
    assert_no_bundled_runtime_plugins(skippr_el)

    latest_index_url = published_latest_manifest_index_url(
        releases_bucket,
        runtime_plugin_release_subdir,
    )
    catalog_entries = load_workspace_plugin_catalog(REPO_ROOT)
    expected_protocol_version = workspace_runtime_protocol_version(REPO_ROOT)
    catalog_by_manifest = {
        entry["manifest_filename"]: entry for entry in catalog_entries
    }
    source_manifest_filenames = set(runtime_source_manifest_filenames(catalog_entries))
    manifest_filenames = runtime_release_manifest_filenames(catalog_entries)
    target = published_target_for_architecture_name(architecture_name)
    full_download_manifests = set(runtime_full_download_manifests(mode))
    latest_index = fetch_json_url(latest_index_url)
    latest_entries_by_manifest = {
        entry["manifest_filename"]: entry
        for entry in latest_index.get("manifests", [])
        if isinstance(entry, dict) and "manifest_filename" in entry
    }
    manifest_urls: dict[str, str] = {}
    expected_versions_by_manifest: dict[str, str] = {}
    used_pinned_plugins: set[str] = set()
    missing_latest_manifests: list[str] = []

    for manifest_filename in manifest_filenames:
        metadata = catalog_by_manifest.get(manifest_filename)
        if metadata is None:
            raise HarnessError(
                f"runtime plugin catalog is missing metadata for {manifest_filename}"
            )
        pinned_version = None
        if runtime_plugin_versions is not None:
            pinned_version = runtime_plugin_versions.get(metadata["plugin_name"])
        if pinned_version:
            manifest_urls[manifest_filename] = published_versioned_manifest_url(
                releases_bucket=releases_bucket,
                runtime_plugin_release_subdir=runtime_plugin_release_subdir,
                entry=metadata,
                version=pinned_version,
            )
            expected_versions_by_manifest[manifest_filename] = pinned_version
            used_pinned_plugins.add(metadata["plugin_name"])
            continue

        latest_entry = latest_entries_by_manifest.get(manifest_filename)
        if latest_entry is None:
            missing_latest_manifests.append(manifest_filename)
            continue
        manifest_url = latest_entry.get("manifest_url")
        if not isinstance(manifest_url, str) or not manifest_url.strip():
            raise HarnessError(
                f"latest manifest index entry for {manifest_filename} is missing manifest_url"
            )
        manifest_urls[manifest_filename] = manifest_url
        expected_versions_by_manifest[manifest_filename] = metadata["package_version"]

    if missing_latest_manifests:
        raise HarnessError(
            "latest runtime plugin manifest index is missing required manifests: "
            + ", ".join(sorted(missing_latest_manifests))
        )
    if runtime_plugin_versions is not None:
        unknown_pins = sorted(set(runtime_plugin_versions) - used_pinned_plugins)
        if unknown_pins:
            raise HarnessError(
                "runtime plugin acceptance does not cover pinned plugins: "
                + ", ".join(unknown_pins)
            )

    temp_root = Path(tempfile.mkdtemp(prefix="skippr_runtime_plugin_acceptance_"))
    runtime_plugin_dir = temp_root / "runtime-plugin-cache"
    runtime_plugin_dir.mkdir(parents=True, exist_ok=True)
    cleanup = True
    try:
        manifests = download_runtime_release_manifests(
            manifest_urls,
            temp_root / "manifests",
        )
        verify_runtime_release_artifacts(
            manifests,
            source_manifests=source_manifest_filenames,
            full_download_manifests=full_download_manifests,
            catalog_by_manifest=catalog_by_manifest,
            expected_versions_by_manifest=expected_versions_by_manifest,
            expected_protocol_version=expected_protocol_version,
            target=target,
            download_dir=temp_root / "verified-artifacts",
        )

        test_env = runtime_acceptance_test_env(skippr_el)
        run_command(
            ["cargo", "test", "--test", "runtime_host_contracts", "--", "--nocapture"],
            env=test_env,
        )
        if mode == "full":
            run_command(
                ["cargo", "test", "--test", "runtime_file_csv_to_file", "--", "--nocapture"],
                env=test_env,
            )

        run_runtime_file_release_smoke(
            skippr_el=skippr_el,
            runtime_plugin_dir=runtime_plugin_dir,
            manifests=manifests,
        )
        if mode == "full":
            run_runtime_postgres_release_smoke(
                skippr_el=skippr_el,
                runtime_plugin_dir=runtime_plugin_dir,
                manifests=manifests,
            )
    except Exception:
        cleanup = False
        print_step(f"Preserving runtime plugin acceptance dir for debugging: {temp_root}")
        raise
    finally:
        if cleanup:
            shutil.rmtree(temp_root, ignore_errors=True)


def wait_for_athena_query(query_execution_id: str, env: dict[str, str]) -> None:
    while True:
        state = capture_text(
            [
                ensure_tool("aws"),
                "athena",
                "get-query-execution",
                "--query-execution-id",
                query_execution_id,
                "--query",
                "QueryExecution.Status.State",
                "--output",
                "text",
            ],
            env=env,
        )
        if state == "SUCCEEDED":
            return
        if state in {"FAILED", "CANCELLED"}:
            capture_text(
                [
                    ensure_tool("aws"),
                    "athena",
                    "get-query-execution",
                    "--query-execution-id",
                    query_execution_id,
                ],
                env=env,
            )
            raise HarnessError(f"Athena query failed: {query_execution_id} state={state}")
        time.sleep(2)


def start_athena_query(
    sql: str,
    *,
    database: str,
    env: dict[str, str],
    output_location: str,
) -> str:
    command = [
        ensure_tool("aws"),
        "athena",
        "start-query-execution",
        "--work-group",
        "bikehire",
        "--query-string",
        sql,
        "--query-execution-context",
        f"Database={database},Catalog=AwsDataCatalog",
        "--result-configuration",
        f"OutputLocation={output_location}",
        "--query",
        "QueryExecutionId",
        "--output",
        "text",
    ]
    return capture_text(command, env=env)


def athena_scalar(
    sql: str,
    *,
    database: str,
    env: dict[str, str],
    output_location: str,
) -> str:
    query_id = start_athena_query(
        sql,
        database=database,
        env=env,
        output_location=output_location,
    )
    wait_for_athena_query(query_id, env)
    return capture_text(
        [
            ensure_tool("aws"),
            "athena",
            "get-query-results",
            "--query-execution-id",
            query_id,
            "--query",
            "ResultSet.Rows[1].Data[0].VarCharValue",
            "--output",
            "text",
        ],
        env=env,
    )


def athena_scan_bytes(
    sql: str,
    *,
    database: str,
    env: dict[str, str],
    output_location: str,
) -> int:
    query_id = start_athena_query(
        sql,
        database=database,
        env=env,
        output_location=output_location,
    )
    wait_for_athena_query(query_id, env)
    value = capture_text(
        [
            ensure_tool("aws"),
            "athena",
            "get-query-execution",
            "--query-execution-id",
            query_id,
            "--query",
            "QueryExecution.Statistics.DataScannedInBytes",
            "--output",
            "text",
        ],
        env=env,
    )
    return int(value)


def assert_table_has_rows(
    *,
    table: str,
    database: str,
    env: dict[str, str],
    output_location: str,
) -> None:
    count_text = athena_scalar(
        f"SELECT count(*) FROM {table}",
        database=database,
        env=env,
        output_location=output_location,
    )
    if not count_text or count_text == "None" or int(count_text) < 1:
        raise HarnessError(f"expected {database}.{table} to contain at least one row")


def assert_deadletter_routing(context: ScenarioContext) -> None:
    env = context.base_env
    assert_table_has_rows(
        table="_dl_deadletters_test",
        database="deadletters",
        env=env,
        output_location=context.assertion_output,
    )
    glue_payload = capture_json(
        [
            ensure_tool("aws"),
            "glue",
            "get-table",
            "--database-name",
            "deadletters",
            "--name",
            "_dl_deadletters_test",
            "--output",
            "json",
        ],
        env=env,
    )
    columns = {
        column["Name"]
        for column in glue_payload["Table"]["StorageDescriptor"]["Columns"]
    }
    for required in ("id", "namespace", "record", "error", "failure_code", "processed_time"):
        if required not in columns:
            raise HarnessError(f"missing expected deadletter column: {required}")


def assert_pruning(context: ScenarioContext) -> None:
    env = context.base_env
    sample_bike_id = athena_scalar(
        "SELECT bike_id FROM bike_hire_many WHERE bike_id IS NOT NULL LIMIT 1",
        database="bikehire",
        env=env,
        output_location=context.assertion_output,
    )
    if not sample_bike_id or sample_bike_id == "None":
        raise HarnessError("failed to sample a bike_id from bike_hire_many")
    escaped_sample_bike_id = sample_bike_id.replace("'", "''")
    full_scan_bytes = athena_scan_bytes(
        "SELECT count(*) FROM bike_hire_many WHERE bike_id IS NOT NULL",
        database="bikehire",
        env=env,
        output_location=context.assertion_output,
    )
    filtered_scan_bytes = athena_scan_bytes(
        "SELECT count(*) FROM bike_hire_many WHERE bike_id = "
        f"'{escaped_sample_bike_id}'",
        database="bikehire",
        env=env,
        output_location=context.assertion_output,
    )
    print_step(f"Athena full scan bytes: {full_scan_bytes}")
    print_step(f"Athena filtered scan bytes: {filtered_scan_bytes}")
    if filtered_scan_bytes >= full_scan_bytes:
        raise HarnessError("expected filtered query to scan fewer bytes than the full query")


def ensure_soda_installed() -> None:
    global SODA_INSTALLED, SODA_VENV_DIR
    if SODA_INSTALLED:
        return
    soda_python = resolve_soda_python()
    venv_dir = Path(tempfile.mkdtemp(prefix="skippr_runtime_e2e_soda_venv_"))
    run_command([soda_python, "-m", "venv", str(venv_dir)])
    venv_bin_dir = venv_dir / ("Scripts" if os.name == "nt" else "bin")
    venv_python = venv_bin_dir / ("python.exe" if os.name == "nt" else "python")
    # Python 3.12+ no longer bundles distutils in fresh venvs; Soda still imports it.
    run_command(
        [str(venv_python), "-m", "pip", "install", "setuptools", "soda-core-athena"]
    )
    SODA_VENV_DIR = venv_dir
    SODA_INSTALLED = True


def soda_executable() -> str:
    if SODA_VENV_DIR is None:
        return "soda"
    venv_bin_dir = SODA_VENV_DIR / ("Scripts" if os.name == "nt" else "bin")
    executable = "soda.exe" if os.name == "nt" else "soda"
    return str(venv_bin_dir / executable)


def resolve_soda_python() -> str:
    candidates = []
    override = os.environ.get("SKIPPR_E2E_SODA_PYTHON")
    if override:
        candidates.append(override)
    candidates.extend(["python3.11", "python3.10", sys.executable, "python3"])

    seen: set[str] = set()
    for candidate in candidates:
        path = (
            candidate
            if os.path.isabs(candidate) and Path(candidate).exists()
            else shutil.which(candidate)
        )
        if not path or path in seen:
            continue
        seen.add(path)
        completed = subprocess.run(
            [path, "-c", "import distutils.util"],
            text=True,
            capture_output=True,
            check=False,
        )
        if completed.returncode == 0:
            return path

    raise HarnessError(
        "could not find a Python interpreter compatible with Soda CLI; "
        "set SKIPPR_E2E_SODA_PYTHON to a Python 3.11-compatible binary"
    )


def run_soda_scan(scan_file: str, dataset: str, env: dict[str, str]) -> None:
    ensure_soda_installed()
    run_command(
        [
            soda_executable(),
            "scan",
            "-d",
            dataset,
            "-c",
            str(REPO_ROOT / "soda" / "configuration.yml"),
            str(REPO_ROOT / "soda" / scan_file),
        ],
        env=env,
    )


def verify_bike_hire_rows(context: ScenarioContext) -> None:
    assert_table_has_rows(
        table="bike_hire",
        database="bikehire",
        env=context.base_env,
        output_location=context.assertion_output,
    )


def verify_bike_hire_many_rows(context: ScenarioContext) -> None:
    assert_table_has_rows(
        table="bike_hire_many",
        database="bikehire",
        env=context.base_env,
        output_location=context.assertion_output,
    )


def verify_bike_hire_s3_wal_many_rows(context: ScenarioContext) -> None:
    assert_table_has_rows(
        table="bike_hire_s3_wal_many",
        database="bikehire",
        env=context.base_env,
        output_location=context.assertion_output,
    )


def verify_soda_bike_hire(context: ScenarioContext) -> None:
    run_soda_scan("bike_hire.yml", "datalake_e2e", context.base_env)


def verify_soda_bike_hire_many(context: ScenarioContext) -> None:
    run_soda_scan("bike_hire_many.yml", "datalake_e2e", context.base_env)


def verify_soda_bike_hire_s3_wal_many(context: ScenarioContext) -> None:
    run_soda_scan("bike_hire_s3_wal_many.yml", "datalake_e2e", context.base_env)


def verify_soda_deadletters(context: ScenarioContext) -> None:
    run_soda_scan("deadletters_test.yml", "datalake_e2e", context.base_env)
    run_soda_scan(
        "deadletters_deadletter_output.yml",
        "deadletters_e2e",
        context.base_env,
    )


VERIFIERS: dict[str, Callable[[ScenarioContext], None]] = {
    "bike_hire_rows": verify_bike_hire_rows,
    "bike_hire_many_rows": verify_bike_hire_many_rows,
    "bike_hire_s3_wal_many_rows": verify_bike_hire_s3_wal_many_rows,
    "bike_hire_many_pruning": assert_pruning,
    "deadletters_athena_routing": assert_deadletter_routing,
    "soda_bike_hire": verify_soda_bike_hire,
    "soda_bike_hire_many": verify_soda_bike_hire_many,
    "soda_bike_hire_s3_wal_many": verify_soda_bike_hire_s3_wal_many,
    "soda_deadletters": verify_soda_deadletters,
}


def delete_glue_database(name: str, env: dict[str, str]) -> None:
    command = [
        ensure_tool("aws"),
        "glue",
        "delete-database",
        "--catalog-id",
        "353855562591",
        "--name",
        name,
    ]
    completed = subprocess.run(
        command,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode == 0:
        return
    stderr = completed.stderr.strip()
    if "EntityNotFoundException" in stderr or "Database not found" in stderr:
        print_step(f"Glue database {name} did not exist; continuing")
        return
    raise HarnessError(
        f"failed to delete Glue database {name}: {stderr or completed.stdout.strip()}"
    )


def purge_dynamodb(env: dict[str, str]) -> None:
    print_step(f"Purging DynamoDB table {DYNAMODB_TABLE}")
    last_evaluated_key: dict | None = None
    deleted = 0
    while True:
        command = [
            ensure_tool("aws"),
            "dynamodb",
            "scan",
            "--table-name",
            DYNAMODB_TABLE,
            "--projection-expression",
            "tenant,#t",
            "--expression-attribute-names",
            json.dumps({"#t": "time"}),
            "--output",
            "json",
        ]
        if last_evaluated_key:
            command.extend(
                [
                    "--exclusive-start-key",
                    json.dumps(last_evaluated_key),
                ]
            )
        completed = subprocess.run(
            command,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        if completed.returncode != 0:
            stderr = completed.stderr.strip()
            stdout = completed.stdout.strip()
            combined = "\n".join(part for part in (stdout, stderr) if part)
            if (
                "ResourceNotFoundException" in combined
                or "Requested resource not found" in combined
            ):
                print_step(f"DynamoDB table {DYNAMODB_TABLE} did not exist; continuing")
                return
            raise HarnessError(
                "failed to scan DynamoDB table "
                f"{DYNAMODB_TABLE}: {combined or f'exit code {completed.returncode}'}"
            )
        payload = json.loads(completed.stdout) if completed.stdout else {}
        for item in payload.get("Items", []):
            key = {"tenant": item["tenant"], "time": item["time"]}
            run_command(
                [
                    ensure_tool("aws"),
                    "dynamodb",
                    "delete-item",
                    "--table-name",
                    DYNAMODB_TABLE,
                    "--key",
                    json.dumps(key),
                ],
                env=env,
            )
            deleted += 1
        last_evaluated_key = payload.get("LastEvaluatedKey")
        if not last_evaluated_key:
            break
    print_step(f"Purged {deleted} DynamoDB items from {DYNAMODB_TABLE}")


def prepare_aws_state(env: dict[str, str], *, skip_dynamodb: bool) -> None:
    ensure_tool("aws")
    print_step("Cleaning shared AWS e2e state")
    run_command(
        [ensure_tool("aws"), "s3", "rm", "s3://skippr-e2e-sample-data-output", "--recursive"],
        env=env,
    )
    run_command(
        [ensure_tool("aws"), "s3", "rm", "s3://skippr-metadata-test", "--recursive"],
        env=env,
    )
    delete_glue_database("bikehire", env)
    delete_glue_database("deadletters", env)
    if not skip_dynamodb:
        purge_dynamodb(env)


def scenario_pipeline_name(scenario: Scenario) -> str:
    pipeline_names = {
        sync_run.pipeline for sync_run in (*scenario.smoke_runs, *scenario.full_runs)
    }
    if len(pipeline_names) != 1:
        raise HarnessError(
            f"scenario {scenario.name} expected a single pipeline name, found {sorted(pipeline_names)}"
        )
    return next(iter(pipeline_names))


def prepare_local_state(scenario: Scenario) -> None:
    data_dir = REPO_ROOT / "data" / f"test_{scenario_pipeline_name(scenario)}"
    if not data_dir.exists():
        return
    print_step(f"Removing local scenario data dir {data_dir}")
    shutil.rmtree(data_dir, ignore_errors=True)


def run_scenario(
    scenario: Scenario,
    *,
    mode: str,
    skippr_el: Path,
    prepare: bool,
    skip_dynamodb: bool,
    runtime_plugin_versions: dict[str, str] | None,
    local_runtime_manifests: LocalStagedRuntimeManifests | None,
) -> None:
    runtime_plugin_dir_path = Path(
        tempfile.mkdtemp(prefix=f"skippr_runtime_e2e_{scenario.name}_")
    )
    cleanup_runtime_dir = True
    try:
        base_env_for_setup = os.environ.copy()
        base_env_for_setup.setdefault("AWS_DEFAULT_REGION", DEFAULT_AWS_REGION)
        if prepare:
            prepare_local_state(scenario)
            prepare_aws_state(base_env_for_setup, skip_dynamodb=skip_dynamodb)

        config_path = materialize_scenario_config(
            scenario,
            runtime_plugin_dir_path,
            runtime_plugin_versions,
            local_runtime_manifests,
        )
        base_env = base_environment(
            skippr_el=skippr_el,
            config_path=config_path,
            runtime_plugin_dir=runtime_plugin_dir_path,
        )
        context = ScenarioContext(
            scenario=scenario,
            skippr_el=skippr_el,
            runtime_plugin_dir=runtime_plugin_dir_path,
            base_env=base_env,
            assertion_output=DEFAULT_ASSERTION_OUTPUT,
        )

        sync_runs = scenario.smoke_runs if mode == "smoke" else scenario.full_runs
        print_step(f"Running scenario {scenario.name} in {mode} mode")
        for sync_run in sync_runs:
            run_sync(skippr_el, sync_run, base_env)

        if local_runtime_manifests is None:
            assert_runtime_plugins_downloaded(runtime_plugin_dir_path)
        else:
            print_step(
                f"Scenario {scenario.name} used local staged manifests from {local_runtime_manifests.manifest_dir}"
            )

        verifier_names = list(scenario.smoke_verifiers)
        if mode == "full":
            verifier_names.extend(scenario.full_verifiers)
        for verifier_name in verifier_names:
            VERIFIERS[verifier_name](context)
    except Exception:
        cleanup_runtime_dir = False
        print_step(f"Preserving runtime plugin dir for debugging: {runtime_plugin_dir_path}")
        raise
    finally:
        if cleanup_runtime_dir:
            shutil.rmtree(runtime_plugin_dir_path, ignore_errors=True)


def add_common_run_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--skippr-el",
        help="Path to the skippr-el binary or artifact directory containing it",
    )
    parser.add_argument(
        "--mode",
        choices=("smoke", "full"),
        default="smoke",
        help="Smoke mode runs the shortest high-signal checks; full mode mirrors CI validations",
    )
    parser.add_argument(
        "--prepare-aws-state",
        action="store_true",
        help="Delete shared S3/Glue state (and DynamoDB unless skipped) before running scenarios",
    )
    parser.add_argument(
        "--skip-dynamodb-purge",
        action="store_true",
        help="Skip the DynamoDB purge during --prepare-aws-state",
    )
    parser.add_argument(
        "--runtime-plugin-version",
        action="append",
        help=(
            "Pin a published runtime plugin version for scenario discovery using "
            "PLUGIN=VERSION; may be provided multiple times"
        ),
    )
    parser.add_argument(
        "--local-runtime-manifest-dir",
        help="Use a local staged runtime manifest directory for the S3/Athena/Glue scenario plugins instead of published discovery",
    )


def list_scenarios() -> None:
    for name in SCENARIOS:
        print(name)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Local and CI harness for runtime-plugin AWS e2e scenarios."
        )
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    list_parser = subparsers.add_parser("list", help="List supported e2e scenarios")
    list_parser.set_defaults(handler=lambda _args: list_scenarios())

    prepare_parser = subparsers.add_parser(
        "prepare-aws-state",
        help="Clean shared AWS e2e state before running scenarios",
    )
    prepare_parser.add_argument(
        "--skip-dynamodb-purge",
        action="store_true",
        help="Skip the DynamoDB purge",
    )
    prepare_parser.set_defaults(handler=None)

    run_parser = subparsers.add_parser(
        "run",
        help="Run one or more runtime-plugin AWS e2e scenarios",
    )
    run_parser.add_argument(
        "scenarios",
        nargs="+",
        choices=tuple(SCENARIOS.keys()),
        help="Scenario names to run in order",
    )
    add_common_run_args(run_parser)
    run_parser.set_defaults(handler=None)

    stage_runtime_release_parser = subparsers.add_parser(
        "stage-local-runtime-release",
        help="Stage the current S3/Athena/Glue runtime plugin binaries into a local release-like manifest tree",
    )
    stage_runtime_release_parser.add_argument(
        "--skippr-el",
        help="Path to the skippr-el binary or artifact directory containing it; used to match debug vs release plugin builds",
    )
    stage_runtime_release_parser.add_argument(
        "--output-dir",
        help="Output directory for the staged local runtime release; defaults to a new temp directory",
    )
    stage_runtime_release_parser.set_defaults(handler=None)

    runtime_plugins_parser = subparsers.add_parser(
        "runtime-plugins",
        help="Verify published runtime plugin manifests and smoke-test released manifests",
    )
    runtime_plugins_parser.add_argument(
        "--skippr-el",
        help="Path to the skippr-el binary or artifact directory containing it",
    )
    runtime_plugins_parser.add_argument(
        "--mode",
        choices=("smoke", "full"),
        default="smoke",
        help="Smoke mode verifies every published source manifest and runs the lightest release smokes; full adds the postgres release smoke and helper regression test",
    )
    runtime_plugins_parser.add_argument(
        "--architecture-name",
        required=True,
        help="Release architecture name such as linux_x86 or macos_arm64",
    )
    runtime_plugins_parser.add_argument(
        "--releases-bucket",
        required=True,
        help="Public S3 bucket containing released runtime plugin assets",
    )
    runtime_plugins_parser.add_argument(
        "--runtime-plugin-version",
        action="append",
        help=(
            "Pin a published runtime plugin version to validate using PLUGIN=VERSION; "
            "unselected plugins resolve from latest"
        ),
    )
    runtime_plugins_parser.add_argument(
        "--runtime-plugin-release-subdir",
        default="runtime-plugins",
        help="Release subdirectory for published runtime plugin assets",
    )
    runtime_plugins_parser.set_defaults(handler=None)

    args = parser.parse_args(argv)

    try:
        if args.command == "list":
            args.handler(args)
            return 0

        if args.command == "prepare-aws-state":
            env = os.environ.copy()
            env.setdefault("AWS_DEFAULT_REGION", DEFAULT_AWS_REGION)
            prepare_aws_state(env, skip_dynamodb=args.skip_dynamodb_purge)
            return 0

        if args.command == "run":
            skippr_el = resolve_skippr_el(args.skippr_el)
            assert_no_bundled_runtime_plugins(skippr_el)
            runtime_plugin_versions = parse_runtime_plugin_versions(
                args.runtime_plugin_version
            )
            if runtime_plugin_versions and args.local_runtime_manifest_dir:
                raise HarnessError(
                    "--runtime-plugin-version cannot be combined with --local-runtime-manifest-dir"
                )
            local_runtime_manifests = (
                load_local_runtime_manifests(Path(args.local_runtime_manifest_dir))
                if args.local_runtime_manifest_dir
                else None
            )
            for scenario_name in args.scenarios:
                run_scenario(
                    SCENARIOS[scenario_name],
                    mode=args.mode,
                    skippr_el=skippr_el,
                    prepare=args.prepare_aws_state,
                    skip_dynamodb=args.skip_dynamodb_purge,
                    runtime_plugin_versions=runtime_plugin_versions,
                    local_runtime_manifests=local_runtime_manifests,
                )
            return 0

        if args.command == "stage-local-runtime-release":
            skippr_el = resolve_skippr_el(args.skippr_el)
            output_dir = (
                Path(args.output_dir)
                if args.output_dir
                else Path(tempfile.mkdtemp(prefix="skippr_local_runtime_release_"))
            )
            local_runtime_manifests = stage_local_runtime_release(
                skippr_el=skippr_el,
                output_dir=output_dir,
            )
            print(local_runtime_manifests.manifest_dir)
            return 0

        if args.command == "runtime-plugins":
            run_runtime_plugin_acceptance(
                skippr_el_arg=args.skippr_el,
                mode=args.mode,
                architecture_name=args.architecture_name,
                releases_bucket=args.releases_bucket,
                runtime_plugin_versions=parse_runtime_plugin_versions(
                    args.runtime_plugin_version
                ),
                runtime_plugin_release_subdir=args.runtime_plugin_release_subdir,
            )
            return 0

        raise HarnessError(f"unsupported command: {args.command}")
    except HarnessError as err:
        print(f"runtime e2e harness failed: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
