#!/usr/bin/env python3

import argparse
import copy
import hashlib
import json
import shutil
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from runtime_plugin_catalog import (
    load_workspace_plugin_catalog,
    manifest_payload_for_catalog_entry,
    versioned_artifact_relative_path,
    versioned_manifest_relative_path,
    workspace_runtime_protocol_version,
)
from runtime_plugin_targets import published_runtime_plugin_targets, resolve_target_artifact

METADATA_REFRESH_QUERY_PARAM = "skippr_metadata_refresh"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def public_url(public_base_url: str, bucket: str, subdir: str, *parts: str) -> str:
    suffix = "/".join(part.strip("/") for part in parts if part)
    if public_base_url:
        base = public_base_url.rstrip("/")
        return f"{base}/{suffix}" if suffix else base
    return f"https://{bucket}.s3.amazonaws.com/releases/{subdir}/{suffix}"


def rewrite_public_url(url: str, public_base_url: str, subdir: str) -> str:
    if not public_base_url or not url:
        return url

    release_prefix = f"/releases/{subdir.strip('/')}/"
    parsed = urllib.parse.urlsplit(url)
    if release_prefix not in parsed.path:
        return url

    relative_path = parsed.path.split(release_prefix, 1)[1]
    return public_url(public_base_url, "", subdir, relative_path)


def latest_manifest_index_url(public_base_url: str, bucket: str, subdir: str) -> str:
    return public_url(public_base_url, bucket, subdir, "latest", "manifest-index.json")


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
        raise SystemExit(f"Failed to load runtime plugin index {index_url}: HTTP {err.code}") from err
    except urllib.error.URLError as err:
        raise SystemExit(f"Failed to load runtime plugin index {index_url}: {err}") from err

    manifests = {}
    for entry in index.get("manifests", []):
        try:
            manifests[entry["manifest_filename"]] = fetch_json_url(entry["manifest_url"])
        except urllib.error.HTTPError as err:
            if err.code in {403, 404}:
                continue
            raise SystemExit(
                f"Failed to load runtime manifest {entry['manifest_url']}: HTTP {err.code}"
            ) from err
        except urllib.error.URLError as err:
            raise SystemExit(
                f"Failed to load runtime manifest {entry['manifest_url']}: {err}"
            ) from err
    return index, manifests


def published_manifest_matches_catalog(published: dict, plugin: dict) -> bool:
    return (
        published.get("version") == plugin["package_version"]
        and published.get("build_checksum") == plugin["checksum"]
    )


def target_binary_name(binary_name: str, target_triple: str) -> str:
    if "windows" in target_triple and not binary_name.endswith(".exe"):
        return f"{binary_name}.exe"
    return binary_name


def artifact_urls_rewritten(manifest: dict, public_base_url: str, subdir: str) -> dict:
    rewritten = copy.deepcopy(manifest)
    artifacts = rewritten.get("artifacts", {})
    for key, artifact in artifacts.items():
        artifact["executable"] = Path(artifact.get("executable", "")).name
        if artifact.get("url"):
            artifact["url"] = rewrite_public_url(artifact["url"], public_base_url, subdir)
        artifacts[key] = artifact
    rewritten["artifacts"] = artifacts
    return rewritten


def main() -> None:
    parser = argparse.ArgumentParser(description="Stage runtime plugin artifacts and publish manifests")
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--subdir", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--public-base-url", default="")
    parser.add_argument("--published-index-url", default="")
    args = parser.parse_args()

    workspace = Path(args.workspace).resolve()
    output_dir = Path(args.output_dir).resolve()
    public_base_url = args.public_base_url.strip()
    catalog_entries = load_workspace_plugin_catalog(workspace)
    publish_targets = published_runtime_plugin_targets(workspace)
    protocol_version = workspace_runtime_protocol_version(workspace)
    published_index_url = args.published_index_url.strip() or latest_manifest_index_url(
        public_base_url, args.bucket, args.subdir
    )
    _published_index, published_manifests = load_latest_published_manifests(published_index_url)

    latest_entries = []
    for entry in catalog_entries:
        published_manifest = published_manifests.get(entry["manifest_filename"])
        can_reuse_published = published_manifest_matches_catalog(published_manifest or {}, entry)
        artifacts = {}

        for target in publish_targets:
            binary_name = target_binary_name(entry["binary_name"], target.triple)
            source_binary = workspace / target.publish_artifact_dir / binary_name
            if source_binary.exists():
                staged_relative_path = versioned_artifact_relative_path(
                    entry, target.triple, binary_name
                )
                staged_binary = output_dir / staged_relative_path
                staged_binary.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source_binary, staged_binary)
                artifacts[target.triple] = {
                    "executable": binary_name,
                    "url": public_url(
                        public_base_url,
                        args.bucket,
                        args.subdir,
                        staged_relative_path.as_posix(),
                    ),
                    "sha256": sha256(source_binary),
                }
                continue

            if can_reuse_published:
                fallback_artifact = resolve_target_artifact(
                    (published_manifest or {}).get("artifacts", {}), target
                )
                if fallback_artifact:
                    fallback_artifact["executable"] = Path(
                        fallback_artifact.get("executable", binary_name)
                    ).name
                    if fallback_artifact.get("url"):
                        fallback_artifact["url"] = rewrite_public_url(
                            fallback_artifact["url"], public_base_url, args.subdir
                        )
                    artifacts[target.triple] = fallback_artifact

        if not artifacts:
            raise SystemExit(
                f"No runtime plugin artifacts were available for {entry['manifest_filename']}"
            )

        manifest_relative_path = versioned_manifest_relative_path(entry)
        output_manifest_path = output_dir / manifest_relative_path
        output_manifest_path.parent.mkdir(parents=True, exist_ok=True)

        if can_reuse_published and not any(
            (workspace / target.publish_artifact_dir / target_binary_name(entry["binary_name"], target.triple)).exists()
            for target in publish_targets
        ):
            manifest_payload = artifact_urls_rewritten(
                published_manifest,
                public_base_url,
                args.subdir,
            )
        else:
            manifest_payload = manifest_payload_for_catalog_entry(
                entry,
                protocol_version=protocol_version,
                artifacts=artifacts,
                build_checksum=entry["checksum"],
            )

        manifest_payload["artifacts"] = artifacts
        manifest_payload["build_checksum"] = entry["checksum"]
        manifest_payload["protocol_version"] = protocol_version

        with output_manifest_path.open("w", encoding="utf-8") as handle:
            json.dump(manifest_payload, handle, indent=2)
            handle.write("\n")

        latest_entries.append(
            {
                "name": entry["manifest_name"],
                "plugin_name": entry["plugin_name"],
                "kind": entry["manifest_kind"],
                "manifest_filename": entry["manifest_filename"],
                "manifest_url": public_url(
                    public_base_url,
                    args.bucket,
                    args.subdir,
                    manifest_relative_path.as_posix(),
                ),
            }
        )

    latest_dir = output_dir / "latest"
    latest_dir.mkdir(parents=True, exist_ok=True)
    latest_index_path = latest_dir / "manifest-index.json"
    with latest_index_path.open("w", encoding="utf-8") as handle:
        json.dump(
            {
                "bundle_version": "latest",
                "manifests": latest_entries,
            },
            handle,
            indent=2,
        )
        handle.write("\n")


if __name__ == "__main__":
    main()
