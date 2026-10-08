"""One merge for `Config` registration, `Config.save`, and `skipprd connect`."""

import os
import subprocess
import sys
import textwrap

import pytest
import skippr
from skippr import (
    Config,
    DataSinkDuckdb,
    DataSinkPostgres,
    DataSourceFile,
    DataSourceS3,
    EnvRef,
    LocalStorage,
    Pipeline,
    S3Storage,
    Transform,
)


def _file_source(tmp_path):
    return DataSourceFile(path=str(tmp_path / "in.json"))


def test_reregistering_merges_in_memory_like_save(tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text(
        textwrap.dedent(
            """\
            pipelines:
              p:
                data_source: data_sources.src
                sync_frequency_seconds: 30
            data_sources:
              src:
                S3:
                  s3_bucket: b
                  s3_prefix: old
                  region: eu-west-1
                  version: "1"
            """
        )
    )
    cfg = Config.load(path)
    src = cfg.data_source("src", DataSourceS3(s3_bucket="b", s3_prefix="new"))
    cfg.pipeline("p", Pipeline(data_source=src))
    in_memory = cfg.to_yaml()
    cfg.save()
    on_disk = Config.load(path).to_yaml()
    assert in_memory == on_disk
    for kept in ["region: eu-west-1", "version: '1'", "sync_frequency_seconds: 30", "s3_prefix: new"]:
        assert kept in in_memory, kept


def test_new_file_does_not_invent_storage_mode(tmp_path):
    cfg = Config().workspace("w")
    path = tmp_path / "skippr.yml"
    cfg.save(path)
    raw = path.read_text()
    assert "skipprd_el_storage_mode" not in raw
    assert Config.load(path).to_yaml() == cfg.to_yaml()


def test_save_writes_a_file_that_loads(tmp_path):
    cfg = Config().storage(S3Storage("bucket"))
    cfg.data_source("src", DataSourceS3(s3_bucket="b", s3_prefix="p"))
    path = tmp_path / "skippr.yml"
    cfg.save(path)
    assert Config.load(path).to_yaml() == cfg.to_yaml()


def test_legacy_aliases_and_removed_pipeline_keys_are_rejected(tmp_path):
    for body in [
        "pipelines:\n  p:\n    input: data_sources.src\n",
        "data_inputs:\n  src:\n    File:\n      path: /tmp\n",
        "pipelines:\n  p:\n    reset_offsets: true\n",
        "pipelines:\n  p:\n    buffer_disk_threshold_bytes: 5\n",
    ]:
        path = tmp_path / "skippr.yml"
        path.write_text(body)
        with pytest.raises(ValueError, match="unknown field"):
            Config.load(path)


def test_save_keeps_dbt_and_vector_sources(tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text("dbt:\n  target_schema: ts\nvector_sources:\n  v:\n    kind: x\n")
    copy = tmp_path / "copy.yml"
    Config.load(path).save(copy)
    raw = copy.read_text()
    assert "target_schema: ts" in raw
    assert "kind: x" in raw


@pytest.mark.parametrize("value", ["${A}hunter2}", "${}", "123456", "hunter2", ""])
def test_secret_fields_accept_only_env_refs_on_save(tmp_path, value):
    path = tmp_path / "skippr.yml"
    rendered = f"'{value}'" if value else "''"
    path.write_text(
        f"data_sinks:\n  db:\n    Postgres:\n      user: u\n      database: d\n      password: {rendered}\n"
    )
    with pytest.raises(ValueError, match=r"\$\{ENV\}"):
        Config.load(path).save()
    if value == "123456":
        path.write_text(
            "data_sinks:\n  db:\n    Postgres:\n      user: u\n      database: d\n      password: 123456\n"
        )
        with pytest.raises(ValueError, match=r"\$\{ENV\}"):
            Config.load(path).save()


def test_invalid_pipeline_raises_instead_of_exiting(tmp_path):
    script = textwrap.dedent(
        f"""
        from skippr import Config, DataSourceFile, LocalStorage, Pipeline, Session, Transform
        c = Config().workspace("w").storage(LocalStorage())
        src = c.data_source("s", DataSourceFile(path={str(tmp_path / "in.json")!r}))
        p = c.pipeline("p", Pipeline(data_source=src, data_dir={str(tmp_path / "d")!r},
                                     transform=Transform(batch_time_unit="day")))
        try:
            Session(p)
        except ValueError as err:
            print("raised:", err)
        """
    )
    out = subprocess.run([sys.executable, "-c", script], capture_output=True, text=True)
    assert out.returncode == 0, out.stderr
    assert "raised:" in out.stdout, out.stdout + out.stderr


@pytest.mark.parametrize(
    ("env", "storage", "data_dir"),
    [
        ({"WAL_STORAGE": "bogus"}, "LocalStorage()", None),
        ({"SKIPPRD_EL_STORAGE_MODE": "bogus"}, None, None),
        ({"SKIPPR_STORE_TYPE": "bogus"}, "LocalStorage()", None),
        ({}, "LocalStorage()", "/dev/null/nope"),
    ],
)
def test_bad_startup_settings_raise_instead_of_exiting(tmp_path, env, storage, data_dir):
    dd = data_dir or str(tmp_path / "d")
    cfg = f'Config().workspace("w").storage({storage})' if storage else 'Config().workspace("w")'
    script = textwrap.dedent(
        f"""
        from skippr import Config, DataSourceFile, LocalStorage, Pipeline, Session
        c = {cfg}
        src = c.data_source("s", DataSourceFile(path={str(tmp_path / "in.json")!r}))
        p = c.pipeline("p", Pipeline(data_source=src, data_dir={dd!r}))
        try:
            Session(p)
        except ValueError as err:
            print("raised:", err)
        """
    )
    clean = {k: v for k, v in os.environ.items() if not k.startswith(("WAL_", "SKIPPR", "DATA_DIR"))}
    out = subprocess.run(
        [sys.executable, "-c", script], capture_output=True, text=True, env={**clean, **env}
    )
    assert out.returncode == 0, out.stdout + out.stderr
    assert "raised:" in out.stdout, out.stdout + out.stderr


def test_removed_store_env_is_rejected(tmp_path, monkeypatch):
    cfg = Config().workspace("w").storage(LocalStorage())
    src = cfg.data_source("s", _file_source(tmp_path))
    ref = cfg.pipeline("p", Pipeline(data_source=src, data_dir=str(tmp_path / "d")))
    monkeypatch.setenv("SKIPPR_OFFSET_STORE", "dynamodb")
    with pytest.raises(ValueError, match="SKIPPR_STORE_TYPE"):
        skippr.Session(ref)


def test_session_resolves_only_its_pipeline(tmp_path, monkeypatch):
    monkeypatch.delenv("SKIPPR_TEST_UNRELATED_PASSWORD", raising=False)
    cfg = Config().workspace("w").storage(LocalStorage())
    src = cfg.data_source("s", _file_source(tmp_path))
    pg = cfg.data_sink(
        "pg",
        DataSinkPostgres(user="u", database="d", password=EnvRef("SKIPPR_TEST_UNRELATED_PASSWORD")),
    )
    cfg.pipeline("other", Pipeline(data_source=src, data_sink=pg))
    mine = cfg.pipeline("mine", Pipeline(data_source=src, data_dir=str(tmp_path / "d")))
    assert skippr.Session(mine).pipeline == "mine"


def test_fresh_config_round_trips_through_save(tmp_path):
    cfg = Config()
    path = tmp_path / "skippr.yml"
    cfg.save(path)
    assert cfg.to_yaml() == Config.load(path).to_yaml()


@pytest.mark.parametrize("name", ["skipprd.json", "skipprd.toml", "skippr.txt"])
def test_save_refuses_non_yaml_paths(tmp_path, name):
    path = tmp_path / name
    with pytest.raises(ValueError, match="YAML"):
        Config().workspace("w").save(path)
    assert not path.exists()


def test_lowercase_plugin_key_saves_as_the_canonical_kind(tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text("data_sources:\n  src:\n    file:\n      path: /tmp/in\n")
    cfg = Config.load(path)
    cfg.data_source("src", DataSourceFile(path="/tmp/other"))
    cfg.save()
    saved = Config.load(path).to_yaml()
    assert "File:" in saved and "file:" not in saved
    assert "path: /tmp/other" in saved


def test_unknown_plugin_kind_is_rejected_on_save(tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text("data_sources:\n  src:\n    NotAPlugin:\n      x: 1\n")
    with pytest.raises(ValueError, match="NotAPlugin"):
        Config.load(path).workspace("w").save()


@pytest.mark.parametrize(
    "body",
    [
        "pipelines:\n  p:\n    transform:\n      flaten_events: true\n",
        "pipelines:\n  p:\n    stats:\n      enabeld: true\n",
        "pipelines:\n  p:\n    semantic_layer:\n      llm_enabeld: true\n",
        "dbt:\n  target_schmea: x\n",
        "skippr:\n  store:\n    type: sled\n    nmae: t\n",
    ],
)
def test_unknown_nested_keys_are_rejected(tmp_path, body):
    path = tmp_path / "skippr.yml"
    path.write_text(body)
    with pytest.raises(ValueError, match="unknown field"):
        Config.load(path)


def test_env_refs_belong_in_string_fields_only(tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text("pipelines:\n  p:\n    sync_frequency_seconds: ${SYNC}\n")
    with pytest.raises(ValueError):
        Config.load(path)


def test_session_does_not_resolve_dbt_or_vector_sources(tmp_path, monkeypatch):
    monkeypatch.delenv("SKIPPR_TEST_UNSET_DBT", raising=False)
    path = tmp_path / "skippr.yml"
    path.write_text(
        "skippr:\n  workspace: w\n  skipprd_el_storage_mode: local\n"
        "dbt:\n  target_schema: ${SKIPPR_TEST_UNSET_DBT}\n"
        "vector_sources:\n  docs:\n    path: ${SKIPPR_TEST_UNSET_DBT}\n"
        f"pipelines:\n  p:\n    data_source: data_sources.s\n    data_dir: {tmp_path / 'd'}\n"
        f"data_sources:\n  s:\n    File:\n      path: {tmp_path / 'in.json'}\n"
    )
    assert skippr.Session(Config.load(path).get_pipeline("p")).pipeline == "p"


def test_reregistering_a_paired_sink_keeps_its_schema_sink_in_step(tmp_path):
    cfg = Config().workspace("w").storage(LocalStorage())
    src = cfg.data_source("s", _file_source(tmp_path))
    cfg.data_sink("lake", DataSinkDuckdb(warehouse="file:///tmp/w1", table_namespace="main"), schema_sink="lake")
    lake = cfg.data_sink("lake", DataSinkDuckdb(warehouse="file:///tmp/w2", table_namespace="main"))
    p = cfg.pipeline("p", Pipeline(data_source=src, data_sink=lake, data_dir=str(tmp_path / "d")))
    raw = cfg.to_yaml()
    assert "w1" not in raw and raw.count("warehouse: file:///tmp/w2") == 2
    assert skippr.Session(p).pipeline == "p"


def test_save_refuses_a_file_whose_pipelines_no_longer_validate(tmp_path):
    path = tmp_path / "skippr.yml"
    ice = textwrap.indent(
        "AthenaIceberg:\n  warehouse: s3://w/\n  glue_database_name: g\n"
        "  athena_workgroup_name: primary\n  athena_results_s3_bucket: r\n  region: eu-west-1\n",
        "    ",
    )
    base = (
        "pipelines:\n  p:\n    data_source: data_sources.src\n    data_sink: data_sinks.ice\n"
        "data_sources:\n  src:\n    File:\n      path: /tmp/in\n"
        "data_sinks:\n  ice:\n    schema_sink: schema_sinks.ice\n" + ice
        + "schema_sinks:\n  ice:\n" + ice
    )
    path.write_text(base)
    cfg = Config()
    src = cfg.data_source("src", DataSourceFile(path="/tmp/in"))
    sink = cfg.data_sink(
        "ice",
        skippr.DataSinkAthenaIceberg(
            warehouse="s3://w/", glue_database_name="g", athena_workgroup_name="primary", athena_results_s3_bucket="r"
        ),
        schema_sink="other",
    )
    cfg.pipeline("p", Pipeline(data_source=src, data_sink=sink))
    with pytest.raises(ValueError, match="must be equal"):
        cfg.save(str(path))
    assert path.read_text() == base


def test_save_never_rewrites_a_pair_the_caller_did_not_touch(tmp_path):
    path = tmp_path / "skippr.yml"
    duck = "    Duckdb:\n      warehouse: file:///tmp/{}\n      table_namespace: main\n"
    base = (
        "data_sinks:\n  lake:\n    schema_sink: schema_sinks.lake\n" + duck.format("w1")
        + "schema_sinks:\n  lake:\n" + duck.format("w2")
    )
    path.write_text(base)
    with pytest.raises(ValueError, match="must be equal"):
        Config.load(str(path)).save()
    assert path.read_text() == base


def _iceberg(**overrides):
    fields = dict(warehouse="s3://w/", glue_database_name="g", athena_workgroup_name="primary", athena_results_s3_bucket="r")
    return skippr.DataSinkAthenaIceberg(**{**fields, **overrides})


def test_shared_schema_sink_matches_a_lowercase_kind_in_the_file(tmp_path):
    path = tmp_path / "skippr.yml"
    body = (
        "      warehouse: s3://w/\n      glue_database_name: g\n"
        "      athena_workgroup_name: primary\n      athena_results_s3_bucket: r\n"
    )
    path.write_text(
        "data_sinks:\n  a:\n    schema_sink: schema_sinks.lake\n    athenaiceberg:\n" + body
        + "schema_sinks:\n  lake:\n    athenaiceberg:\n" + body
    )
    cfg = Config.load(str(path))
    cfg.data_sink("b", _iceberg(), schema_sink="lake")
    assert cfg.to_yaml().count("schema_sink: schema_sinks.lake") == 2


def test_a_shared_paired_sink_is_one_config_across_every_member(tmp_path):
    cfg = Config()
    cfg.data_sink("a", _iceberg(region="eu-west-1"), schema_sink="shared")
    cfg.data_sink("b", _iceberg(), schema_sink="shared")
    assert cfg.to_yaml().count("region: eu-west-1") == 3
    cfg.data_sink("a", _iceberg(region="us-east-1"), schema_sink="shared")
    raw = cfg.to_yaml()
    assert raw.count("region: us-east-1") == 3 and "eu-west-1" not in raw


def test_a_loaded_pair_that_already_differs_is_refused_not_overwritten(tmp_path):
    path = tmp_path / "skippr.yml"
    duck = "    Duckdb:\n      warehouse: file:///tmp/w\n      table_namespace: {}\n"
    path.write_text(
        "data_sinks:\n  d:\n    schema_sink: schema_sinks.s\n" + duck.format("a")
        + "schema_sinks:\n  s:\n" + duck.format("b")
    )
    cfg = Config.load(str(path))
    with pytest.raises(ValueError, match="must be equal"):
        cfg.data_sink("d", DataSinkDuckdb(warehouse="file:///tmp/w", table_namespace="c"), schema_sink="s")


def test_two_sinks_cannot_share_a_duckdb_namespace(tmp_path):
    cfg = Config()
    cfg.data_sink("a", DataSinkDuckdb(warehouse="file:///tmp/w", table_namespace="main"))
    with pytest.raises(ValueError, match="reuses warehouse"):
        cfg.data_sink("b", DataSinkDuckdb(warehouse="file:///tmp/w", table_namespace="main"))
    assert "  b:" not in cfg.to_yaml()


@pytest.mark.parametrize(
    "build",
    [
        lambda: S3Storage(""),
        lambda: skippr.DynamoDbStore(" "),
        lambda: skippr.CloudTablesStore(""),
        lambda: Config().workspace(""),
        lambda: Config().tenant(" "),
        lambda: Config().wal_s3_bucket(""),
    ],
)
def test_empty_names_are_rejected_where_they_are_set(build):
    with pytest.raises(ValueError, match="must not be empty"):
        build()
