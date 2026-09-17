import skipprd


def _cfg():
    return {
        "skippr": {"workspace": "quickstart", "skipprd_el_storage_mode": "local"},
        "pipelines": {
            "p1": {"data_source": "data_sources.sample"},
            "p2": {"data_source": "data_sources.sample"},
        },
        "data_sources": {"sample": {"S3": {"s3_bucket": "b", "s3_prefix": "p"}}},
    }


def test_two_sessions_do_not_share_pipeline():
    cfg = _cfg()
    a = skipprd.Session(config=cfg, pipeline="p1")
    b = skipprd.Session(config=cfg, pipeline="p2")
    assert a.pipeline == "p1"
    assert b.pipeline == "p2"


def test_session_pipeline_is_immutable():
    s = skipprd.Session(config=_cfg(), pipeline="p1")
    try:
        s.pipeline = "p2"
    except AttributeError:
        assert s.pipeline == "p1"
        return
    raise AssertionError("Session.pipeline must be set only in the constructor")


def test_kwargs_constructor_matches_yml_fields():
    s = skipprd.Session(
        pipeline="p1",
        skippr={"workspace": "quickstart"},
        pipelines={"p1": {"data_source": "data_sources.sample"}},
        data_sources={"sample": {"S3": {"s3_bucket": "b", "s3_prefix": "p"}}},
    )
    assert s.pipeline == "p1"
    result = s.doctor()
    assert result["ok"] is True
    messages = [c["message"] for c in result["checks"]]
    assert any("WAL is the dataset" in m for m in messages)


def test_df_and_query_return_pyarrow_table():
    import pyarrow as pa

    s = skipprd.Session(config=_cfg(), pipeline="p1")
    table = s.df()
    assert isinstance(table, pa.Table)
    queried = s.query("SELECT 1 AS n")
    assert isinstance(queried, pa.Table)
