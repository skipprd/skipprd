#!/usr/bin/env python3

import argparse
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from runtime_plugin_catalog import (
    load_workspace_plugin_catalog,
    workspace_runtime_protocol_version,
)
from runtime_plugin_targets import (
    artifact_matches_build_environment,
    published_runtime_plugin_targets,
    resolve_target_artifact,
)


DEFAULT_PUBLISHED_INDEX_URL = (
    "https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json"
)
METADATA_REFRESH_QUERY_PARAM = "skippr_metadata_refresh"


def fresh_metadata_url(url: str) -> str:
    parsed = urllib.parse.urlsplit(url)
    query = urllib.parse.parse_qsl(parsed.query, keep_blank_values=True)
    query.append((METADATA_REFRESH_QUERY_PARAM, str(time.time_ns())))
    return urllib.parse.urlunsplit(
        parsed._replace(query=urllib.parse.urlencode(query))
    )


def metadata_request(url: str) -> urllib.request.Request:
    return urllib.request.Request(
        fresh_metadata_url(url),
        headers={
            "Cache-Control": "no-cache, no-store, max-age=0",
            "Pragma": "no-cache",
        },
    )


def fetch_json_url(url: str) -> dict:
    with urllib.request.urlopen(metadata_request(url), timeout=30) as response:
        return json.load(response)


def load_latest_published_manifests(index_url: str) -> tuple[dict, dict[str, dict]]:
    if not index_url:
        return {}, {}

    try:
        index = fetch_json_url(index_url)
    except urllib.error.HTTPError as err:
        if err.code in {403, 404}:
            return {}, {}
        raise SystemExit(f"failed to load published runtime plugin index {index_url}: HTTP {err.code}") from err
    except urllib.error.URLError as err:
        raise SystemExit(f"failed to load published runtime plugin index {index_url}: {err}") from err

    manifests = {}
    for entry in index.get("manifests", []):
        try:
            manifests[entry["manifest_filename"]] = fetch_json_url(entry["manifest_url"])
        except urllib.error.HTTPError as err:
            if err.code in {403, 404}:
                continue
            raise SystemExit(
                f"failed to load published runtime manifest {entry['manifest_url']}: HTTP {err.code}"
            ) from err
        except urllib.error.URLError as err:
            raise SystemExit(
                f"failed to load published runtime manifest {entry['manifest_url']}: {err}"
            ) from err

    return index, manifests


def published_artifacts_with_stale_build_environment(
    published: dict, publish_targets: list
) -> list[str]:
    artifacts = published.get("artifacts", {})
    stale_targets = []
    for target in publish_targets:
        artifact = resolve_target_artifact(artifacts, target)
        if artifact and not artifact_matches_build_environment(artifact, target):
            stale_targets.append(target.triple)
    return stale_targets


def published_manifest_matches_catalog(
    published: dict,
    published_index: dict,
    published_index_entry: dict,
    plugin: dict,
    publish_targets: list,
    protocol_version: int,
) -> bool:
    sdk_build_fingerprint = plugin["sdk_build_fingerprint"]
    return (
        published.get("version") == plugin["package_version"]
        and published.get("protocol_version") == protocol_version
        and published.get("sdk_build_fingerprint") == sdk_build_fingerprint
        and published_index.get("sdk_build_fingerprint") == sdk_build_fingerprint
        and published_index_entry.get("sdk_build_fingerprint")
        == sdk_build_fingerprint
        and published.get("build_checksum") == plugin["checksum"]
        and not published_artifacts_with_stale_build_environment(
            published, publish_targets
        )
    )


def stale_sdk_build_metadata_reason(
    published: dict,
    published_index: dict,
    published_index_entry: dict,
    plugin: dict,
) -> str | None:
    expected = plugin["sdk_build_fingerprint"]
    stale_locations = [
        location
        for location, actual in (
            ("published manifest", published.get("sdk_build_fingerprint")),
            ("manifest index", published_index.get("sdk_build_fingerprint")),
            (
                "manifest index entry",
                published_index_entry.get("sdk_build_fingerprint"),
            ),
        )
        if actual != expected
    ]
    if not stale_locations:
        return None
    return (
        "runtime SDK build fingerprint changed or is missing in "
        + ", ".join(stale_locations)
    )


def plan_runtime_plugin_release(
    catalog: list[dict],
    publish_targets: list,
    protocol_version: int,
    published_index: dict,
    published_manifests: dict[str, dict],
) -> dict:
    published_index_entries = {
        entry.get("manifest_filename"): entry
        for entry in published_index.get("manifests", [])
        if isinstance(entry, dict) and entry.get("manifest_filename")
    }
    selected = []
    decision_reasons = {}
    for plugin in catalog:
        published = published_manifests.get(plugin["manifest_filename"])
        if published is None:
            selected.append(plugin["package_name"])
            decision_reasons[plugin["package_name"]] = "not yet published"
            continue

        published_version = published.get("version", "")
        published_protocol_version = published.get("protocol_version")
        published_checksum = published.get("build_checksum", "")
        if published_version != plugin["package_version"]:
            selected.append(plugin["package_name"])
            decision_reasons[plugin["package_name"]] = (
                f"version changed from {published_version or '<missing>'} to "
                f"{plugin['package_version']}"
            )
            continue

        if published_protocol_version != protocol_version:
            selected.append(plugin["package_name"])
            decision_reasons[plugin["package_name"]] = (
                "runtime protocol changed from "
                f"{published_protocol_version or '<missing>'} to {protocol_version}"
            )
            continue

        sdk_build_reason = stale_sdk_build_metadata_reason(
            published,
            published_index,
            published_index_entries.get(plugin["manifest_filename"], {}),
            plugin,
        )
        if sdk_build_reason is not None:
            selected.append(plugin["package_name"])
            decision_reasons[plugin["package_name"]] = sdk_build_reason
            continue

        if published_checksum != plugin["checksum"]:
            selected.append(plugin["package_name"])
            decision_reasons[plugin["package_name"]] = (
                "build checksum changed for existing plugin version"
            )
            continue

        stale_build_targets = published_artifacts_with_stale_build_environment(
            published, publish_targets
        )
        if stale_build_targets:
            selected.append(plugin["package_name"])
            decision_reasons[plugin["package_name"]] = (
                "published artifact build environment changed or is missing for "
                + ", ".join(stale_build_targets)
            )

    selected = sorted(set(selected))
    build_all = len(selected) == len(catalog)
    if not published_manifests:
        reason = "no published runtime plugin baseline found; build every runtime plugin"
    elif not selected:
        reason = (
            "all runtime plugins already published at matching version, protocol, "
            "SDK build fingerprint, build checksum, and build environment"
        )
    else:
        reason = (
            "build runtime plugins whose published version, protocol, "
            "SDK build fingerprint, build checksum, or build environment is stale"
        )

    return {
        "build_all": build_all,
        "packages": selected,
        "reason": reason,
        "package_reasons": decision_reasons,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Plan which runtime plugin packages should be rebuilt for a release"
    )
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--published-index-url", default=DEFAULT_PUBLISHED_INDEX_URL)
    args = parser.parse_args()

    workspace = Path(args.workspace).resolve()
    catalog = load_workspace_plugin_catalog(workspace)
    publish_targets = published_runtime_plugin_targets(workspace)
    protocol_version = workspace_runtime_protocol_version(workspace)
    published_index, published_manifests = load_latest_published_manifests(
        args.published_index_url
    )
    plan = plan_runtime_plugin_release(
        catalog,
        publish_targets,
        protocol_version,
        published_index,
        published_manifests,
    )

    json.dump(plan, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
