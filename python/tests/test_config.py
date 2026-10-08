import pytest
import skippr
from skippr import (
    Config,
    DataSinkAthena,
    DataSinkAthenaIceberg,
    DataSinkDuckdb,
    DataSinkPostgres,
    DataSinkSkipprLake,
    DataSinkSkipprLakeWarehouseObjectStoreR2,
    DataSourceFile,
    DataSourceGoogleSerpRanks,
    DataSourceGoogleSerpRanksTargetEntry,
    DataSourceHttpClient,
    DataSourceHttpClientDataSourceHttpAuthConfig,
    DataSourceOtlp,
    DataSourcePostgres,
    DataSourceS3,
    DynamoDbStore,
    EnvRef,
    LocalStorage,
    Pipeline,
    S3Storage,
    SchemaSinkGlue,
    Transform,
)


def _s3():
    return DataSourceS3(s3_bucket="b", s3_prefix="p")


def test_removed_names_are_gone():
    for name in [
        "Connect",
        "StorageMode",
        "OffsetStore",
        "SkipprStore",
        "SkipprRoot",
        "DataSource",
        "DataSink",
        "SchemaSink",
        "workspace",
        "tenant",
        "storage_mode",
    ]:
        assert not hasattr(skippr, name), name
    for field in ["reset_offsets", "reset_metadata", "buffer_disk_threshold_bytes"]:
        assert not hasattr(Pipeline, field), field


def test_builder_round_trips_through_yaml(tmp_path):
    cfg = Config().workspace("bikehire").storage(LocalStorage())
    src = cfg.data_source("sample", _s3())
    cfg.pipeline("p", Pipeline(data_source=src, sync_frequency_seconds=60))
    path = tmp_path / "skippr.yml"
    cfg.save(path)
    raw = path.read_text()
    assert "workspace: bikehire" in raw
    assert "skipprd_el_storage_mode: local" in raw
    assert "data_source: data_sources.sample" in raw
    assert "s3_bucket: b" in raw
    assert "sync_frequency_seconds: 60" in raw
    loaded = Config.load(path)
    assert loaded.path == str(path)
    assert loaded.get_pipeline("p").name == "p"
    loaded.save()
    assert path.read_text() == raw


def test_empty_config_renders_no_registries():
    raw = Config().to_yaml()
    for key in ["pipelines", "data_sources", "data_sinks", "deadletter_sinks", "schema_sinks"]:
        assert key not in raw


def test_save_merges_and_keeps_unset_keys(tmp_path):
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
    cfg = Config()
    cfg.data_source("src_a", DataSourceS3(s3_bucket="keep-me", s3_prefix="new"))
    cfg.save(path)
    raw = path.read_text()
    assert "src_b" in raw
    assert "workspace: demo" in raw
    assert "version:" in raw
    assert "s3_prefix: new" in raw


def test_save_rejects_plugin_kind_change_in_file(tmp_path):
    path = tmp_path / "skippr.yml"
    Config().data_source("src", _s3()).config.save(path)
    with pytest.raises(ValueError, match="already uses"):
        Config().data_source("src", DataSourceFile(path="/tmp/in")).config.save(path)


def test_registration_rejects_plugin_kind_change():
    cfg = Config()
    cfg.data_source("src", _s3())
    with pytest.raises(ValueError, match="already uses"):
        cfg.data_source("src", DataSourceFile(path="/tmp/in"))


def test_save_without_path_needs_an_origin():
    with pytest.raises(ValueError, match="save\\(path\\)"):
        Config().save()


def test_load_keeps_env_refs_unresolved(tmp_path, monkeypatch):
    monkeypatch.setenv("PG_PASSWORD", "hunter2")
    path = tmp_path / "skippr.yml"
    cfg = Config()
    cfg.data_sink(
        "db",
        DataSinkPostgres(user="u", database="d", password=EnvRef("PG_PASSWORD")),
    )
    cfg.save(path)
    loaded = Config.load(path).to_yaml()
    assert "${PG_PASSWORD}" in loaded
    assert "hunter2" not in loaded


def test_discover_starts_empty_and_saves_to_discovered_file(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    cfg = Config.discover()
    assert cfg.path is not None and cfg.path.endswith("skippr.yml")
    cfg.workspace("found").save()
    assert "workspace: found" in (tmp_path / "skippr.yml").read_text()
    assert "workspace: found" in Config.discover().to_yaml()


def test_local_storage_removes_bucket(tmp_path):
    path = tmp_path / "skippr.yml"
    Config().storage(S3Storage("state")).save(path)
    assert "skippr_s3_bucket: state" in path.read_text()
    Config().storage(LocalStorage()).save(path)
    raw = path.read_text()
    assert "skipprd_el_storage_mode: local" in raw
    assert "skippr_s3_bucket" not in raw


def test_store_writes_type_and_name():
    raw = Config().store(DynamoDbStore("offsets")).to_yaml()
    assert "type: dynamodb" in raw
    assert "name: offsets" in raw
    assert "offset_store" not in raw


def test_storage_and_store_reject_wrong_types():
    with pytest.raises(TypeError, match="LocalStorage"):
        Config().storage("local")  # type: ignore[arg-type]
    with pytest.raises(TypeError, match="SledStore"):
        Config().store(LocalStorage())  # type: ignore[arg-type]


def test_env_ref_validates_name():
    assert EnvRef("OK_1").name == "OK_1"
    for bad in ["", "1X", "A-B", "${A}"]:
        with pytest.raises(ValueError):
            EnvRef(bad)


def test_secret_fields_reject_plaintext():
    with pytest.raises(TypeError):
        DataSinkPostgres(user="u", database="d", password="hunter2")  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        DataSourceOtlp(auth_token="hunter2")  # type: ignore[arg-type]


def test_secret_fields_persist_env_ref():
    cfg = Config()
    cfg.data_source("otel", DataSourceOtlp(auth_token=EnvRef("OTLP_TOKEN")))
    cfg.data_source(
        "http",
        DataSourceHttpClient(
            url="https://ex",
            auth=DataSourceHttpClientDataSourceHttpAuthConfig(token=EnvRef("HTTP_TOKEN")),
        ),
    )
    raw = cfg.to_yaml()
    assert "auth_token: ${OTLP_TOKEN}" in raw
    assert "token: ${HTTP_TOKEN}" in raw


def test_required_fields_are_required():
    with pytest.raises(TypeError):
        DataSourceS3(s3_bucket="b")  # type: ignore[call-arg]
    with pytest.raises(TypeError):
        Pipeline()  # type: ignore[call-arg]


def test_required_fields_reject_none_on_set():
    src = DataSourceS3(s3_bucket="b", s3_prefix="p")
    with pytest.raises(TypeError):
        src.s3_bucket = None  # type: ignore[assignment]
    assert src.s3_bucket == "b"


def test_any_fields_keep_exact_integers_and_reject_what_json_cannot_hold():
    big = 2**64 - 1
    t = Transform(inject_fields={"n": big, "neg": -(2**63), "nested": [{"f": 1.5, "b": True, "z": None}], "t": (1, "a")})
    assert t.inject_fields == {"n": big, "neg": -(2**63), "nested": [{"f": 1.5, "b": True, "z": None}], "t": [1, "a"]}
    with pytest.raises(ValueError, match="64-bit"):
        Transform(inject_fields={"n": 10**30})
    with pytest.raises(ValueError, match="finite"):
        Transform(inject_fields={"n": float("nan")})
    with pytest.raises(TypeError, match="keys must be str"):
        Transform(inject_fields={"nested": {1: "x"}})
    with pytest.raises(TypeError, match="set"):
        Transform(inject_fields={"n": {1, 2}})


def test_list_and_map_setters_accept_what_the_stub_declares():
    serp = DataSourceGoogleSerpRanks(keywords=["k"], targets=[])
    serp.keywords = ["a", "b"]
    assert list(serp.keywords) == ["a", "b"]
    http = DataSourceHttpClient(url="https://example.com")
    http.headers = {"a": "b"}
    assert dict(http.headers or {}) == {"a": "b"}


def test_digit_strings_stay_strings(tmp_path):
    cfg = Config()
    cfg.data_sink(
        "ice",
        skippr.DataSinkAthenaIceberg(
            warehouse="s3://w/",
            glue_database_name="g",
            athena_workgroup_name="primary",
            athena_results_s3_bucket="r",
            catalog_id="012345678912",
        ),
        schema_sink="ice",
    )
    path = tmp_path / "skippr.yml"
    cfg.save(str(path))
    assert path.read_text().count("catalog_id: '012345678912'") == 2
    assert Config.load(str(path)).to_yaml().count("catalog_id: '012345678912'") == 2


def test_lists_numbers_and_nested_classes_persist_typed():
    cfg = Config()
    cfg.data_source(
        "pg",
        DataSourcePostgres(
            host="localhost",
            password=EnvRef("POSTGRES_PASSWORD"),
            tables=["orders", "items"],
        ),
    )
    cfg.data_source(
        "local",
        DataSourceFile(path="/tmp/in", batch_size_bytes=1048576, batch_size_seconds=5),
    )
    cfg.data_source(
        "serp",
        DataSourceGoogleSerpRanks(
            keywords=["skippr"],
            targets=[DataSourceGoogleSerpRanksTargetEntry(site="example.com", aliases=["ex"])],
        ),
    )
    raw = cfg.to_yaml()
    assert "- orders" in raw and "- items" in raw
    assert "batch_size_bytes: 1048576" in raw
    assert "batch_size_seconds: 5" in raw
    assert "site: example.com" in raw
    assert "- ex" in raw


def test_literal_fields_validate_values():
    DataSourceGoogleSerpRanks(keywords=[], targets=[], device="mobile")
    with pytest.raises(ValueError, match="desktop"):
        DataSourceGoogleSerpRanks(keywords=[], targets=[], device="tablet")  # type: ignore[arg-type]


def test_tagged_enum_variant_serializes_tag():
    cfg = Config()
    cfg.data_sink(
        "lake",
        DataSinkSkipprLake(
            warehouse="s3://wh/",
            catalog_table="cat",
            object_store=DataSinkSkipprLakeWarehouseObjectStoreR2(
                endpoint="https://r2",
                access_key_id="k",
                secret_access_key=EnvRef("OBJECTS_SECRET_ACCESS_KEY"),
            ),
        ),
    )
    raw = cfg.to_yaml()
    assert "type: r2" in raw
    assert "secret_access_key: ${OBJECTS_SECRET_ACCESS_KEY}" in raw


def test_nested_classes_are_frozen_and_top_level_is_mutable():
    target = DataSourceGoogleSerpRanksTargetEntry(site="a")
    with pytest.raises(AttributeError):
        target.site = "b"  # type: ignore[misc]
    src = _s3()
    src.s3_prefix = "q"
    assert src.s3_prefix == "q"


def test_paired_sink_registers_matching_schema_sink():
    cfg = Config()
    cfg.data_sink(
        "warehouse",
        DataSinkAthenaIceberg(
            warehouse="s3://wh/",
            glue_database_name="analytics",
            athena_workgroup_name="primary",
            athena_results_s3_bucket="results",
        ),
        schema_sink="warehouse_schema",
    )
    raw = cfg.to_yaml()
    assert "schema_sink: schema_sinks.warehouse_schema" in raw
    assert raw.count("glue_database_name: analytics") == 2
    assert cfg.get_schema_sink("warehouse_schema").name == "warehouse_schema"


def test_paired_sink_schema_sink_must_be_a_name():
    cfg = Config()
    schema = cfg.schema_sink(
        "glue",
        SchemaSinkGlue(
            s3_bucket="out",
            s3_prefix="p",
            athena_workgroup_name="wg",
            athena_results_s3_bucket="out",
            glue_database_name="db",
        ),
    )
    with pytest.raises(TypeError, match="as a name"):
        cfg.data_sink(
            "lake",
            DataSinkDuckdb(warehouse="file:///tmp/lake", table_namespace="bronze"),
            schema_sink=schema,  # type: ignore[arg-type]
        )


def test_unpaired_sink_links_schema_sink_ref():
    cfg = Config()
    glue = cfg.schema_sink(
        "glue",
        SchemaSinkGlue(
            s3_bucket="out",
            s3_prefix="p",
            athena_workgroup_name="wg",
            athena_results_s3_bucket="out",
            glue_database_name="db",
        ),
    )
    sink = cfg.data_sink(
        "lake",
        DataSinkAthena(
            s3_bucket="out",
            s3_prefix="p",
            athena_workgroup_name="wg",
            athena_results_s3_bucket="out",
        ),
        schema_sink=glue,
    )
    cfg.pipeline("p", Pipeline(data_source=cfg.data_source("src", _s3()), data_sink=sink))
    raw = cfg.to_yaml()
    assert "schema_sink: schema_sinks.glue" in raw
    assert "data_sink: data_sinks.lake" in raw
    with pytest.raises(TypeError, match="SchemaSinkRef"):
        cfg.data_sink(
            "lake2",
            DataSinkAthena(
                s3_bucket="out",
                s3_prefix="p",
                athena_workgroup_name="wg",
                athena_results_s3_bucket="out",
            ),
            schema_sink="glue",  # type: ignore[arg-type]
        )


def test_refs_from_another_config_are_rejected():
    a = Config()
    b = Config()
    src = a.data_source("src", _s3())
    with pytest.raises(ValueError, match="different Config"):
        b.pipeline("p", Pipeline(data_source=src))
    glue = a.schema_sink(
        "glue",
        SchemaSinkGlue(
            s3_bucket="out",
            s3_prefix="p",
            athena_workgroup_name="wg",
            athena_results_s3_bucket="out",
            glue_database_name="db",
        ),
    )
    with pytest.raises(ValueError, match="different Config"):
        b.data_sink(
            "lake",
            DataSinkAthena(
                s3_bucket="out",
                s3_prefix="p",
                athena_workgroup_name="wg",
                athena_results_s3_bucket="out",
            ),
            schema_sink=glue,
        )


def test_wrong_plugin_role_is_a_type_error():
    cfg = Config()
    with pytest.raises(TypeError):
        cfg.data_source("x", DataSinkDuckdb(warehouse="w", table_namespace="n"))  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        cfg.data_sink("x", _s3())  # type: ignore[call-overload]


def test_entry_names_are_checked():
    with pytest.raises(ValueError, match="no '.'"):
        Config().data_source("a.b", _s3())
    with pytest.raises(ValueError):
        Config().data_source(" ", _s3())


def test_pipeline_transform_is_typed():
    cfg = Config()
    src = cfg.data_source("src", _s3())
    cfg.pipeline("p", Pipeline(data_source=src, transform=Transform(flatten_events=True)))
    assert "flatten_events: true" in cfg.to_yaml()


def test_getters_return_refs_or_raise_key_error():
    cfg = Config()
    src = cfg.data_source("src", _s3())
    ref = cfg.pipeline("p", Pipeline(data_source=src))
    assert cfg.get_pipeline("p").name == ref.name == "p"
    assert cfg.get_data_source("src").config is cfg
    for getter in [
        cfg.get_pipeline,
        cfg.get_data_source,
        cfg.get_data_sink,
        cfg.get_deadletter_sink,
        cfg.get_schema_sink,
    ]:
        with pytest.raises(KeyError):
            getter("missing")
