"""Python blocks in the docs that use `skippr` must use the shipped API."""

import re
from pathlib import Path

import pytest
import skippr

ROOT = Path(__file__).resolve().parents[2]
DOCS = sorted((ROOT / "docs" / "docs").rglob("*.md")) + [ROOT / "README.md"]
BLOCK = re.compile(r"```python[^\n]*\n(.*?)```", re.S)
USES_SKIPPR = re.compile(
    r"import skippr\b|from skippr import|\bskippr\.[A-Z]|\bcfg\.|\b(Config|Session)[.(]"
    r"|\bs\.(df|query|doctor|sync|discover)\("
)
REMOVED = [
    ".connect(",
    "StorageMode",
    "OffsetStore",
    "SkipprRoot",
    "Session(pipeline=",
    "config_file=",
    "skippr.workspace(",
    "skippr.tenant(",
    "DataSource.",
    "DataSink.",
    "SchemaSink.",
    ".pipelines(",
    ".data_sources(",
    "Config(**",
    ".storage_mode(",
    ".config(",
]


def _blocks():
    for path in DOCS:
        for index, match in enumerate(BLOCK.finditer(path.read_text(encoding="utf-8"))):
            code = match.group(1)
            if USES_SKIPPR.search(code):
                yield pytest.param(code, id=f"{path.relative_to(ROOT)}#{index}")


BLOCKS = list(_blocks())


def test_docs_have_python_blocks():
    assert BLOCKS


@pytest.mark.parametrize("code", BLOCKS)
def test_doc_block_compiles(code):
    compile(code, "<doc>", "exec")


@pytest.mark.parametrize("code", BLOCKS)
def test_doc_block_avoids_removed_api(code):
    for name in REMOVED:
        assert name not in code, name


@pytest.mark.parametrize("code", BLOCKS)
def test_doc_block_names_exist(code):
    imported = re.findall(r"from skippr import ([^\n]+)", code)
    names = {n.strip() for line in imported for n in line.split(",") if n.strip()}
    names |= set(re.findall(r"\bskippr\.([A-Za-z_]\w*)", code))
    for name in names:
        assert hasattr(skippr, name), name


YAML_BLOCK = re.compile(r"```ya?ml[^\n]*\n(.*?)```", re.S)
ROOT_KEYS = {
    "skippr",
    "pipelines",
    "data_sources",
    "data_sinks",
    "deadletter_sinks",
    "schema_sinks",
    "dbt",
    "vector_sources",
}


def _config_yaml_blocks():
    for path in DOCS:
        text = path.read_text(encoding="utf-8")
        for index, match in enumerate(YAML_BLOCK.finditer(text)):
            code = match.group(1)
            keys = set(re.findall(r"^([A-Za-z_]\w*):", code, re.M))
            if keys and keys <= ROOT_KEYS:
                yield pytest.param(code, id=f"{path.relative_to(ROOT)}#yaml{index}")


CONFIG_YAML_BLOCKS = list(_config_yaml_blocks())


def test_docs_have_config_yaml_blocks():
    assert len(CONFIG_YAML_BLOCKS) > 10


@pytest.mark.parametrize("code", CONFIG_YAML_BLOCKS)
def test_doc_config_yaml_loads(code, tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text(code, encoding="utf-8")
    skippr.Config.load(path)
