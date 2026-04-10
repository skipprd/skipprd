#!/usr/bin/env python3

import argparse
import hashlib
import json
import shutil
from pathlib import Path


TARGET_ARTIFACTS = {
    "linux-x86_64": "skippr-el-linux_x86",
    "darwin-aarch64": "skippr-el-macos_arm64",
    "windows-x86_64": "skippr-el-windows_x86",
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def public_url(bucket: str, release_tag: str, subdir: str, target: str, filename: str) -> str:
    return (
        f"https://{bucket}.s3.amazonaws.com/releases/"
        f"{release_tag}/{subdir}/{target}/{filename}"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description="Stage runtime plugin artifacts and public manifests")
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--release-tag", required=True)
    parser.add_argument("--subdir", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--output-dir", required=True)
    args = parser.parse_args()

    workspace = Path(args.workspace).resolve()
    output_dir = Path(args.output_dir).resolve()
    manifests_dir = workspace / "runtime_plugins" / "manifests"
    output_manifests_dir = output_dir / "manifests"
    output_manifests_dir.mkdir(parents=True, exist_ok=True)

    for manifest_path in sorted(manifests_dir.glob("*.json")):
        with manifest_path.open("r", encoding="utf-8") as handle:
            manifest = json.load(handle)

        manifest["version"] = args.release_tag
        manifest["executable"] = Path(manifest["executable"]).name

        staged_artifacts = {}
        existing_artifacts = manifest.get("artifacts", {})
        for target, artifact_dir_name in TARGET_ARTIFACTS.items():
            artifact = dict(existing_artifacts.get(target, {}))
            binary_name = Path(artifact.get("executable", manifest["executable"])).name
            source_binary = workspace / artifact_dir_name / binary_name
            if not source_binary.exists():
                continue

            staged_target_dir = output_dir / target
            staged_target_dir.mkdir(parents=True, exist_ok=True)
            staged_binary = staged_target_dir / binary_name
            if not staged_binary.exists():
                shutil.copy2(source_binary, staged_binary)

            artifact["executable"] = binary_name
            artifact["url"] = public_url(
                args.bucket,
                args.release_tag,
                args.subdir,
                target,
                binary_name,
            )
            artifact["sha256"] = sha256(source_binary)
            staged_artifacts[target] = artifact

        if not staged_artifacts:
            raise SystemExit(
                f"No runtime plugin binaries were found for manifest {manifest_path.name}"
            )

        manifest["artifacts"] = staged_artifacts

        output_path = output_manifests_dir / manifest_path.name
        with output_path.open("w", encoding="utf-8") as handle:
            json.dump(manifest, handle, indent=2)
            handle.write("\n")


if __name__ == "__main__":
    main()
