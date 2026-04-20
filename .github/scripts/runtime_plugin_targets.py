#!/usr/bin/env python3

import json
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class RuntimePluginTarget:
    triple: str
    aliases: tuple[str, ...]
    publish_artifact_dir: str | None

    @property
    def manifest_keys(self) -> tuple[str, ...]:
        return (self.triple, *self.aliases)


def load_runtime_plugin_targets(workspace: Path) -> list[RuntimePluginTarget]:
    targets_path = workspace / "runtime_plugins" / "targets.json"
    with targets_path.open("r", encoding="utf-8") as handle:
        payload = json.load(handle)

    return [
        RuntimePluginTarget(
            triple=entry["triple"],
            aliases=tuple(entry.get("aliases", [])),
            publish_artifact_dir=entry.get("publish_artifact_dir"),
        )
        for entry in payload["targets"]
    ]


def published_runtime_plugin_targets(workspace: Path) -> list[RuntimePluginTarget]:
    return [
        target for target in load_runtime_plugin_targets(workspace) if target.publish_artifact_dir
    ]


def resolve_target_artifact(artifacts: dict, target: RuntimePluginTarget) -> dict:
    for key in target.manifest_keys:
        artifact = artifacts.get(key)
        if artifact:
            return dict(artifact)
    return {}
