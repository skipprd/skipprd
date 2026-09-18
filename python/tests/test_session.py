import pytest
import skippr
from skippr import StorageMode


def _cfg(**overrides):
    body = {
        "skippr": {"workspace": "quickstart", "skipprd_el_storage_mode": "local"},
        "pipelines": {
            "p1": {"data_source": "data_sources.sample"},
            "p2": {"data_source": "data_sources.sample"},
        },
        "data_sources": {"sample": {"S3": {"s3_bucket": "b", "s3_prefix": "p"}}},
    }
    body.update(overrides)
    return skippr.Config(**body)


def test_session_requires_pipeline():
    with pytest.raises(TypeError):
        skippr.Session()


def test_session_rejects_string_config():
    with pytest.raises(TypeError):
        skippr.Session(pipeline="p1", config="skippr.yml")


def test_two_sessions_do_not_share_pipeline():
    cfg = _cfg()
    a = skippr.Session(pipeline="p1", config=cfg)
    b = skippr.Session(pipeline="p2", config=cfg)
    assert a.pipeline == "p1"
    assert b.pipeline == "p2"


def test_session_pipeline_is_immutable():
    s = skippr.Session(pipeline="p1", config=_cfg())
    try:
        s.pipeline = "p2"
    except AttributeError:
        assert s.pipeline == "p1"
        return
    raise AssertionError("Session.pipeline must be set only in the constructor")


def test_config_object_matches_yml_fields():
    cfg = skippr.Config() \
        .workspace("quickstart") \
        .storage_mode(StorageMode.LOCAL) \
        .pipelines({"p1": {"data_source": "data_sources.sample"}}) \
        .data_sources({"sample": {"S3": {"s3_bucket": "b", "s3_prefix": "p"}}})
    s = skippr.Session(pipeline="p1", config=cfg)
    assert s.pipeline == "p1"
    result = s.doctor()
    assert result["ok"] is True
    messages = [c["message"] for c in result["checks"]]
    assert any("WAL is the dataset" in m for m in messages)


def test_session_config_chained_applies_on_run():
    cfg = _cfg()
    s = skippr.Session(pipeline="p1").config(cfg)
    result = s.doctor()
    assert result["ok"] is True
    messages = [c["message"] for c in result["checks"]]
    assert any("WAL is the dataset" in m for m in messages)


def test_session_auto_discovers_yml(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    (tmp_path / "skippr.yml").write_text(
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
    s = skippr.Session(pipeline="p1")
    result = s.doctor()
    assert result["ok"] is True


def test_df_and_query_return_pyarrow_table():
    import pyarrow as pa

    s = skippr.Session(pipeline="p1", config=_cfg())
    table = s.df()
    assert isinstance(table, pa.Table)
    queried = s.query("SELECT 1 AS n")
    assert isinstance(queried, pa.Table)
