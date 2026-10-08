import pyarrow as pa
import pytest
import skippr
from skippr import Config, DataSourceS3, LocalStorage, Pipeline


def _config():
    cfg = Config().workspace("quickstart").storage(LocalStorage())
    src = cfg.data_source("sample", DataSourceS3(s3_bucket="b", s3_prefix="p"))
    p1 = cfg.pipeline("p1", Pipeline(data_source=src))
    p2 = cfg.pipeline("p2", Pipeline(data_source=src))
    return cfg, p1, p2


def test_session_requires_a_pipeline_ref():
    _, p1, _ = _config()
    with pytest.raises(TypeError):
        skippr.Session()  # type: ignore[call-arg]
    with pytest.raises(TypeError):
        skippr.Session("p1")  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        skippr.Session(pipeline="p1", config=p1.config)  # type: ignore[call-arg]


def test_two_sessions_do_not_share_pipeline():
    _, p1, p2 = _config()
    assert skippr.Session(p1).pipeline == "p1"
    assert skippr.Session(p2).pipeline == "p2"


def test_session_pipeline_is_immutable():
    _, p1, _ = _config()
    s = skippr.Session(p1)
    with pytest.raises(AttributeError):
        s.pipeline = "p2"  # type: ignore[misc]


def test_session_doctor_reads_the_built_config():
    _, p1, _ = _config()
    result = skippr.Session(p1).doctor()
    assert result["ok"] is True
    assert set(result) == {"ok", "checks"}
    assert any("WAL is the dataset" in c["message"] for c in result["checks"])


def test_session_from_loaded_file(tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text(
        """
skippr:
  workspace: quickstart
  skipprd_el_storage_mode: local
pipelines:
  p1:
    data_source: data_sources.sample
data_sources:
  sample:
    S3:
      s3_bucket: b
      s3_prefix: p
"""
    )
    result = skippr.Session(Config.load(path).get_pipeline("p1")).doctor()
    assert result["ok"] is True


def test_session_resolves_env_refs_and_fails_on_missing(monkeypatch):
    monkeypatch.delenv("SKIPPR_TEST_MISSING_BUCKET", raising=False)
    cfg = Config().workspace("quickstart").storage(LocalStorage())
    cfg.wal_s3_bucket("${SKIPPR_TEST_MISSING_BUCKET}")
    src = cfg.data_source("sample", DataSourceS3(s3_bucket="b", s3_prefix="p"))
    ref = cfg.pipeline("p1", Pipeline(data_source=src))
    with pytest.raises(ValueError, match="SKIPPR_TEST_MISSING_BUCKET"):
        skippr.Session(ref)
    monkeypatch.setenv("SKIPPR_TEST_MISSING_BUCKET", "wal")
    assert skippr.Session(ref).pipeline == "p1"


def test_df_and_query_return_pyarrow_table():
    _, p1, _ = _config()
    s = skippr.Session(p1)
    assert isinstance(s.df(), pa.Table)
    assert isinstance(s.query("SELECT 1 AS n"), pa.Table)


def test_session_refuses_a_loaded_file_whose_sinks_share_a_namespace(tmp_path):
    path = tmp_path / "skippr.yml"
    sink = "    Duckdb:\n      warehouse: file:///tmp/w\n      table_namespace: main\n"
    path.write_text(
        "pipelines:\n"
        "  p1:\n    data_source: data_sources.src\n    data_sink: data_sinks.a\n"
        "  p2:\n    data_source: data_sources.src\n    data_sink: data_sinks.b\n"
        "data_sources:\n  src:\n    S3:\n      s3_bucket: b\n      s3_prefix: p\n"
        "data_sinks:\n  a:\n" + sink + "  b:\n" + sink
    )
    cfg = Config.load(path)
    with pytest.raises(ValueError, match="reuses warehouse"):
        skippr.Session(cfg.get_pipeline("p1"))
