"""skipprd ELT engine: build a typed `Config`, then run a `Session` on one of its pipelines."""

import os
from collections.abc import Mapping, Sequence
from typing import Any, Literal, TypeAlias, final, overload

import pyarrow

@final
class EnvRef:
    """A `${NAME}` environment reference.

    Secret fields accept only `EnvRef`, so plaintext secrets cannot be written
    to YAML. The variable is read when a `Session` starts."""

    def __new__(cls, name: str) -> EnvRef: ...
    @property
    def name(self) -> str: ...

@final
class LocalStorage:
    """Engine state on the local filesystem (`skipprd_el_storage_mode: local`).
    Saving removes any `skippr_s3_bucket`."""

    def __new__(cls) -> LocalStorage: ...

@final
class S3Storage:
    """Engine state in S3 (`skipprd_el_storage_mode: s3`, `skippr_s3_bucket`)."""

    def __new__(cls, bucket: str) -> S3Storage: ...
    @property
    def bucket(self) -> str: ...

Storage: TypeAlias = LocalStorage | S3Storage

@final
class SledStore:
    """Local sled SkipprStore (`store.type: sled`)."""

    def __new__(cls) -> SledStore: ...

@final
class DynamoDbStore:
    """DynamoDB SkipprStore (`store.type: dynamodb`, `store.name`)."""

    def __new__(cls, table: str) -> DynamoDbStore: ...
    @property
    def table(self) -> str: ...

@final
class CloudTablesStore:
    """Skippr Cloud Tables SkipprStore (`store.type: cloud-tables`, `store.name`)."""

    def __new__(cls, table: str) -> CloudTablesStore: ...
    @property
    def table(self) -> str: ...

Store: TypeAlias = SledStore | DynamoDbStore | CloudTablesStore

@final
class DataSourceRef:
    """A registered `data_sources.<name>` entry of one `Config`."""

    @property
    def name(self) -> str: ...
    @property
    def config(self) -> Config: ...

@final
class DataSinkRef:
    """A registered `data_sinks.<name>` entry of one `Config`."""

    @property
    def name(self) -> str: ...
    @property
    def config(self) -> Config: ...

@final
class DeadletterSinkRef:
    """A registered `deadletter_sinks.<name>` entry of one `Config`."""

    @property
    def name(self) -> str: ...
    @property
    def config(self) -> Config: ...

@final
class SchemaSinkRef:
    """A registered `schema_sinks.<name>` entry of one `Config`."""

    @property
    def name(self) -> str: ...
    @property
    def config(self) -> Config: ...

@final
class PipelineRef:
    """A registered `pipelines.<name>` entry of one `Config`. Pass it to `Session`."""

    @property
    def name(self) -> str: ...
    @property
    def config(self) -> Config: ...

@final
class Config:
    """A skipprd config, built from typed plugin and pipeline classes.

    `save` merge-writes into the YAML file the same way `skipprd connect` does:
    keys you set replace the file's values, keys you did not set are kept, and
    an entry cannot change plugin kind."""

    def __new__(cls) -> Config: ...
    @staticmethod
    def load(path: str | os.PathLike[str]) -> Config:
        """Read a YAML file. `${ENV}` references stay unresolved until a `Session` starts."""
        ...
    @staticmethod
    def discover() -> Config:
        """Load the discovered `skippr.yml`, or start an empty config that saves to it."""
        ...
    @property
    def path(self) -> str | None:
        """The file this config was loaded from, used by `save()` and for `.env` lookup."""
        ...
    def save(self, path: str | os.PathLike[str] | None = None) -> None:
        """Merge-write to `path`, or to the loaded file."""
        ...
    def to_yaml(self) -> str: ...
    def workspace(self, value: str) -> Config: ...
    def tenant(self, value: str) -> Config: ...
    def storage(self, value: Storage) -> Config: ...
    def store(self, value: Store) -> Config: ...
    def wal_s3_bucket(self, value: str) -> Config: ...
    def data_source(self, name: str, config: DataSourceConfig) -> DataSourceRef: ...
    @overload
    def data_sink(
        self, name: str, config: PairedDataSinkConfig, *, schema_sink: str | None = None
    ) -> DataSinkRef:
        """Paired sinks (AthenaIceberg, SkipprLake, Duckdb) register the same config
        as `schema_sinks.<schema_sink>`."""
        ...
    @overload
    def data_sink(
        self,
        name: str,
        config: UnpairedDataSinkConfig,
        *,
        schema_sink: SchemaSinkRef | None = None,
    ) -> DataSinkRef: ...
    @overload
    def deadletter_sink(
        self, name: str, config: PairedDataSinkConfig, *, schema_sink: str | None = None
    ) -> DeadletterSinkRef: ...
    @overload
    def deadletter_sink(
        self,
        name: str,
        config: UnpairedDataSinkConfig,
        *,
        schema_sink: SchemaSinkRef | None = None,
    ) -> DeadletterSinkRef: ...
    def schema_sink(self, name: str, config: SchemaSinkConfig) -> SchemaSinkRef: ...
    def pipeline(self, name: str, pipeline: Pipeline) -> PipelineRef:
        """Register a pipeline. Its refs must come from this config."""
        ...
    def get_pipeline(self, name: str) -> PipelineRef: ...
    def get_data_source(self, name: str) -> DataSourceRef: ...
    def get_data_sink(self, name: str) -> DataSinkRef: ...
    def get_deadletter_sink(self, name: str) -> DeadletterSinkRef: ...
    def get_schema_sink(self, name: str) -> SchemaSinkRef: ...

@final
class Session:
    """The engine bound to one pipeline. It snapshots the pipeline's config, with
    `${ENV}` references resolved, when it is created."""

    def __new__(cls, pipeline: PipelineRef) -> Session: ...
    @property
    def pipeline(self) -> str: ...
    def doctor(self) -> dict[str, Any]: ...
    def discover(self) -> None: ...
    def sync(self, once: bool = False) -> None: ...
    def query(self, sql: str) -> pyarrow.Table: ...
    def df(self, name: str | None = None) -> pyarrow.Table: ...
