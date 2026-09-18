import pytest
import skippr
from skippr import DataSource, StorageMode


def test_session_requires_pipeline():
    with pytest.raises(TypeError):
        skippr.Session()


def test_session_pipeline_only_uses_discovered_yml(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    skippr.workspace("bikehire")
    s = skippr.Session(pipeline="bikehire")
    s.connect().data_source(DataSource.S3).name("sample").s3_bucket("b").s3_prefix("p")
    raw = (tmp_path / "skippr.yml").read_text()
    assert "data_sources.sample" in raw


def test_workspace_and_connect_persist(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("bikehire", config=str(path)).storage_mode(StorageMode.LOCAL)
    s = skippr.Session(pipeline="bikehire", config_file=str(path))
    s.connect().data_source(DataSource.S3).name("sample").s3_bucket("b").s3_prefix("p")
    raw = path.read_text()
    assert "workspace: bikehire" in raw
    assert "skipprd_el_storage_mode: local" in raw
    assert "s3_bucket: b" in raw
    assert "data_sources.sample" in raw


def test_connect_keeps_sibling_pipeline(tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text(
        """
skippr:
  workspace: demo
pipelines:
  a:
    data_source: data_sources.src_a
  b:
    data_source: data_sources.src_b
data_sources:
  src_a:
    S3:
      s3_bucket: keep-me
      s3_prefix: old
      version: "1"
  src_b:
    S3:
      s3_bucket: other
      s3_prefix: p
"""
    )
    s = skippr.Session(pipeline="a", config_file=str(path))
    s.connect().data_source(DataSource.S3).name("src_a").s3_bucket("keep-me").s3_prefix("new")
    raw = path.read_text()
    assert "src_b" in raw
    assert "version:" in raw
    assert "s3_prefix: new" in raw or 's3_prefix: "new"' in raw


def test_plaintext_secret_is_rejected(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    with pytest.raises(ValueError, match=r"\$\{ENV\}"):
        s.connect().data_sink(skippr.DataSink.Postgres).name("db").host("localhost").password(
            "hunter2"
        )


def test_postgres_password_persists_env_ref(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("bikehire", config=str(path))
    s = skippr.Session(pipeline="bikehire", config_file=str(path))
    s.connect().data_sink(skippr.DataSink.Postgres).name("warehouse").host("localhost").user(
        "skippr"
    ).password("${POSTGRES_PASSWORD}").database("analytics")
    raw = path.read_text()
    assert "${POSTGRES_PASSWORD}" in raw
    assert "hunter2" not in raw
    assert "password:" in raw


def test_auth_token_path_follows_plugin(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_source(DataSource.Otlp).name("otel").auth_token("${OTLP_TOKEN}")
    otlp = path.read_text()
    assert "auth_token:" in otlp
    assert "${OTLP_TOKEN}" in otlp
    assert "\n    auth:" not in otlp
    with pytest.raises(ValueError, match=r"\$\{ENV\}"):
        s.connect().data_source(DataSource.Otlp).name("otel").auth_token("hunter2")
    s.connect().data_source(DataSource.HttpClient).name("http").url("https://ex").auth_token(
        "${HTTP_TOKEN}"
    )
    raw = path.read_text()
    http = raw.split("HttpClient:", 1)[1]
    assert "token:" in http
    assert "${HTTP_TOKEN}" in http


def test_numeric_connect_fields_persist_as_yaml_numbers(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_source(DataSource.File).name("local").path("/tmp/in").format("json").batch_size_bytes(
        "1048576"
    ).batch_size_seconds("5")
    raw = path.read_text()
    assert "batch_size_bytes: 1048576" in raw
    assert "batch_size_seconds: 5" in raw
    assert "batch_size_bytes: '1048576'" not in raw
    assert 'batch_size_bytes: "1048576"' not in raw


def test_schema_sink_attaches_to_data_sink(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_sink(skippr.DataSink.Athena).name("lake").s3_bucket("out").s3_prefix(
        "p"
    ).athena_workgroup_name("bikehire").athena_results_s3_bucket("out")
    s.connect().schema_sink(skippr.SchemaSink.Glue).name("glue").s3_bucket("out").s3_prefix(
        "p"
    ).athena_workgroup_name("bikehire").athena_results_s3_bucket("out").glue_database_name(
        "bikehire"
    )
    raw = path.read_text()
    assert "schema_sink: schema_sinks.glue" in raw
    assert "data_sink: data_sinks.lake" in raw
    assert "pipelines:\n  p:\n    schema_sink:" not in raw
