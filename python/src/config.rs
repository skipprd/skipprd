//! Typed `skippr.Config`: the one Python authoring path for skipprd config.
//! Registration returns role-typed refs; `save` uses the `connect` merge-writer.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ::skipprd::connect;
use ::skipprd::connect::SinkRegistry;
use ::skipprd::helpers::configuration::{is_env_name, Config, Registry, Skippr, ENV_NAME_GRAMMAR};
use ::skipprd::helpers::plugin_config::{DataSinkEntry, PluginConfigEntry};
use ::skipprd::helpers::wal_storage::{ElStorageMode, SkipprStore, SkipprStoreKind};
use pyo3::exceptions::{PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBool, PyDict, PyFloat, PyInt, PyList, PyModule, PyString, PyTuple};
use serde::Serialize;

use crate::value_to_py;

pub(crate) fn value_err(err: String) -> PyErr {
    PyValueError::new_err(err)
}

/// A `${NAME}` environment reference: the only value a secret field accepts.
#[pyclass(frozen, from_py_object, name = "EnvRef", module = "skippr")]
#[derive(Clone)]
pub struct PyEnvRef {
    #[pyo3(get)]
    name: String,
}

#[pymethods]
impl PyEnvRef {
    #[new]
    fn new(name: String) -> PyResult<Self> {
        if !is_env_name(&name) {
            return Err(PyValueError::new_err(format!(
                "EnvRef name {name:?} must match {ENV_NAME_GRAMMAR}"
            )));
        }
        Ok(Self { name })
    }

    fn __repr__(&self) -> String {
        format!("EnvRef({:?})", self.name)
    }
}

impl Serialize for PyEnvRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("${{{}}}", self.name))
    }
}

/// A `serde_json::Value` field: any JSON-serializable Python value.
#[derive(Clone)]
pub struct PyAnyValue(serde_json::Value);

impl Serialize for PyAnyValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// Nesting ceiling for a `PyAnyValue`, so a cyclic or hostile value fails
/// instead of exhausting the stack.
const MAX_ANY_DEPTH: usize = 64;

fn py_to_value(obj: &Bound<'_, PyAny>, depth: usize) -> PyResult<serde_json::Value> {
    use serde_json::Value;
    if depth > MAX_ANY_DEPTH {
        return Err(PyValueError::new_err(format!(
            "config value nests deeper than {MAX_ANY_DEPTH} levels"
        )));
    }
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(flag) = obj.cast::<PyBool>() {
        return Ok(Value::Bool(flag.is_true()));
    }
    if obj.is_instance_of::<PyInt>() {
        if let Ok(n) = obj.extract::<i64>() {
            return Ok(n.into());
        }
        return obj.extract::<u64>().map(Value::from).map_err(|_| {
            PyValueError::new_err(format!(
                "integer {obj} does not fit the 64-bit range a config value holds"
            ))
        });
    }
    if let Ok(float) = obj.cast::<PyFloat>() {
        return serde_json::Number::from_f64(float.value())
            .map(Value::Number)
            .ok_or_else(|| PyValueError::new_err(format!("{obj} is not a finite number")));
    }
    if obj.is_instance_of::<PyString>() {
        return obj.extract::<String>().map(Value::String);
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (key, value) in dict.iter() {
            let key: String = key.extract().map_err(|_| {
                PyTypeError::new_err(format!(
                    "config value keys must be str, got {}",
                    type_name(&key)
                ))
            })?;
            map.insert(key, py_to_value(&value, depth + 1)?);
        }
        return Ok(Value::Object(map));
    }
    if obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>() {
        return obj
            .try_iter()?
            .map(|item| py_to_value(&item?, depth + 1))
            .collect::<PyResult<Vec<_>>>()
            .map(Value::Array);
    }
    Err(PyTypeError::new_err(format!(
        "{} is not a config value; use None, bool, int, float, str, list, tuple, or dict",
        type_name(obj)
    )))
}

impl<'a, 'py> FromPyObject<'a, 'py> for PyAnyValue {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        py_to_value(&obj, 0).map(Self)
    }
}

impl<'py> IntoPyObject<'py> for PyAnyValue {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(value_to_py(py, self.0)?.into_bound(py))
    }
}

fn type_name(obj: &Bound<'_, PyAny>) -> String {
    obj.get_type()
        .qualname()
        .map(|name| name.to_string())
        .unwrap_or_else(|_| "object".into())
}

fn check_literal(alias: &str, values: &[&str], value: String) -> PyResult<String> {
    if values.contains(&value.as_str()) {
        return Ok(value);
    }
    let expected = values
        .iter()
        .map(|v| format!("{v:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(PyValueError::new_err(format!(
        "{value:?} is not a valid {alias}; expected one of {expected}"
    )))
}

/// `Literal["a"] | Literal["b"]`: the runtime twin of the stub's
/// `Literal["a", "b"]`, which type checkers read as that union.
fn literal_alias<'py>(py: Python<'py>, values: &[&str]) -> PyResult<Bound<'py, PyAny>> {
    let literal = py.import("typing")?.getattr("Literal")?;
    let members = values
        .iter()
        .map(|value| literal.get_item(*value))
        .collect::<PyResult<Vec<_>>>()?;
    union_alias(members)
}

fn union_alias<'py>(
    types: impl IntoIterator<Item = Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let mut types = types.into_iter();
    let first = types
        .next()
        .ok_or_else(|| PyValueError::new_err("a union alias needs a member"))?;
    types.try_fold(first, |acc, ty| acc.call_method1("__or__", (ty,)))
}

fn serialize_tagged<S: serde::Serializer, T: Serialize>(
    serializer: S,
    tag: &str,
    wire: &str,
    value: &T,
) -> Result<S::Ok, S::Error> {
    use serde::ser::Error;
    let serde_json::Value::Object(fields) =
        serde_json::to_value(value).map_err(S::Error::custom)?
    else {
        return Err(S::Error::custom(
            "a tagged variant must serialize to a mapping",
        ));
    };
    let mut out = serde_json::Map::new();
    out.insert(tag.to_string(), wire.into());
    out.extend(fields);
    out.serialize(serializer)
}

fn config_repr<T: Serialize>(class: &str, value: &T) -> PyResult<String> {
    let serde_json::Value::Object(fields) =
        serde_json::to_value(value).map_err(|e| PyValueError::new_err(e.to_string()))?
    else {
        return Ok(format!("{class}()"));
    };
    let args = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("{class}({args})"))
}

fn plugin_entry<T: Serialize>(plugin_name: &str, config: &T) -> PyResult<PluginConfigEntry> {
    let config = serde_json::to_value(config).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(PluginConfigEntry {
        plugin_name: plugin_name.to_string(),
        config,
    })
}

fn wrong_config_kind(role: &str, obj: &Bound<'_, PyAny>) -> PyErr {
    PyTypeError::new_err(format!(
        "expected a skippr.{role}* config, got {}",
        type_name(obj)
    ))
}

/// A data sink config, split by whether its schema sink must share it.
pub enum SinkConfig {
    Paired(PluginConfigEntry),
    Unpaired(PluginConfigEntry),
}

/// A string enum field: a `str` validated against the serde names.
macro_rules! config_literal {
    ($name:ident, $alias:literal, [$($value:literal),+ $(,)?]) => {
        #[derive(Clone, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            const VALUES: &'static [&'static str] = &[$($value),+];
        }

        impl<'a, 'py> FromPyObject<'a, 'py> for $name {
            type Error = PyErr;

            fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
                let value: String = obj.extract().map_err(Into::<PyErr>::into)?;
                check_literal($alias, Self::VALUES, value).map(Self)
            }
        }

        impl<'py> IntoPyObject<'py> for $name {
            type Target = PyString;
            type Output = Bound<'py, PyString>;
            type Error = std::convert::Infallible;

            fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
                Ok(PyString::new(py, &self.0))
            }
        }
    };
}

/// A `#[serde(tag = ...)]` enum field: one frozen class per variant.
macro_rules! config_union {
    ($name:ident, $alias:literal, $tag:literal, { $($variant:ident($class:ty) = $wire:literal),+ $(,)? }) => {
        #[derive(Clone)]
        pub enum $name {
            $($variant($class)),+
        }

        impl $name {
            fn type_alias(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
                union_alias([$(py.get_type::<$class>().into_any()),+])
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    $(Self::$variant(value) => serialize_tagged(serializer, $tag, $wire, value)),+
                }
            }
        }

        impl<'a, 'py> FromPyObject<'a, 'py> for $name {
            type Error = PyErr;

            fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
                $(
                    if let Ok(value) = obj.cast::<$class>() {
                        return Ok(Self::$variant(value.get().clone()));
                    }
                )+
                Err(PyTypeError::new_err(format!(
                    "expected a skippr.{}, got {}",
                    $alias,
                    type_name(&obj)
                )))
            }
        }

        impl<'py> IntoPyObject<'py> for $name {
            type Target = PyAny;
            type Output = Bound<'py, PyAny>;
            type Error = PyErr;

            fn into_pyobject(self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
                match self {
                    $(Self::$variant(value) => Ok(Bound::new(py, value)?.into_any())),+
                }
            }
        }
    };
}

include!("connect_generated.rs");

/// A registered entry of one `Config`. Registry refs serialize as
/// `<registry>.<name>` inside a pipeline.
macro_rules! config_ref {
    ($name:ident, $py:literal) => {
        #[pyclass(frozen, from_py_object, name = $py, module = "skippr")]
        pub struct $name {
            config: Py<PyConfig>,
            #[pyo3(get)]
            name: String,
        }

        impl Clone for $name {
            fn clone(&self) -> Self {
                Python::attach(|py| Self {
                    config: self.config.clone_ref(py),
                    name: self.name.clone(),
                })
            }
        }

        #[pymethods]
        impl $name {
            #[getter]
            fn config(&self, py: Python<'_>) -> Py<PyConfig> {
                self.config.clone_ref(py)
            }

            fn __repr__(&self) -> String {
                format!("{}({:?})", $py, self.name)
            }
        }
    };
    ($name:ident, $py:literal, $registry:expr) => {
        config_ref!($name, $py);

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&$registry.reference(&self.name))
            }
        }
    };
}

config_ref!(PyDataSourceRef, "DataSourceRef", Registry::DataSources);
config_ref!(PyDataSinkRef, "DataSinkRef", Registry::DataSinks);
config_ref!(
    PyDeadletterSinkRef,
    "DeadletterSinkRef",
    Registry::DeadletterSinks
);
config_ref!(PySchemaSinkRef, "SchemaSinkRef", Registry::SchemaSinks);
config_ref!(PyPipelineRef, "PipelineRef");

impl PyPipelineRef {
    /// This pipeline's scope of the config, with `${ENV}` resolved.
    pub(crate) fn resolved_config(&self, py: Python<'_>) -> PyResult<Config> {
        let config = self.config.borrow(py);
        config
            .inner
            .scoped_to(&self.name)
            .and_then(|scoped| scoped.resolved(config.origin.as_deref()))
            .map_err(value_err)
    }

    pub(crate) fn pipeline_name(&self) -> &str {
        &self.name
    }
}

#[pyclass(frozen, name = "LocalStorage", module = "skippr")]
pub struct PyLocalStorage;

#[pymethods]
impl PyLocalStorage {
    #[new]
    fn new() -> Self {
        Self
    }
}

#[pyclass(frozen, name = "S3Storage", module = "skippr")]
pub struct PyS3Storage {
    #[pyo3(get)]
    bucket: String,
}

#[pymethods]
impl PyS3Storage {
    #[new]
    fn new(bucket: String) -> PyResult<Self> {
        Ok(Self {
            bucket: non_empty("S3Storage bucket", bucket)?,
        })
    }
}

#[pyclass(frozen, name = "SledStore", module = "skippr")]
pub struct PySledStore;

#[pymethods]
impl PySledStore {
    #[new]
    fn new() -> Self {
        Self
    }
}

#[pyclass(frozen, name = "DynamoDbStore", module = "skippr")]
pub struct PyDynamoDbStore {
    #[pyo3(get)]
    table: String,
}

#[pymethods]
impl PyDynamoDbStore {
    #[new]
    fn new(table: String) -> PyResult<Self> {
        Ok(Self {
            table: non_empty("DynamoDbStore table", table)?,
        })
    }
}

#[pyclass(frozen, name = "CloudTablesStore", module = "skippr")]
pub struct PyCloudTablesStore {
    #[pyo3(get)]
    table: String,
}

#[pymethods]
impl PyCloudTablesStore {
    #[new]
    fn new(table: String) -> PyResult<Self> {
        Ok(Self {
            table: non_empty("CloudTablesStore table", table)?,
        })
    }
}

/// An unresolved engine config plus the file it came from.
#[pyclass(name = "Config", module = "skippr")]
pub struct PyConfig {
    inner: Config,
    origin: Option<PathBuf>,
}

fn check_entry_name(name: &str) -> PyResult<()> {
    connect::check_entry_name(name).map_err(value_err)
}

fn non_empty(what: &str, value: String) -> PyResult<String> {
    if value.trim().is_empty() {
        return Err(PyValueError::new_err(format!("{what} must not be empty")));
    }
    Ok(value)
}

fn sink_entries(
    registry: SinkRegistry,
    config: &Config,
) -> Option<&BTreeMap<String, DataSinkEntry>> {
    match registry {
        SinkRegistry::Data => config.data_sinks.as_ref(),
        SinkRegistry::Deadletter => config.deadletter_sinks.as_ref(),
    }
}

fn entries_update(
    name: &str,
    entry: PluginConfigEntry,
) -> Option<BTreeMap<String, PluginConfigEntry>> {
    Some(BTreeMap::from([(name.to_string(), entry)]))
}

impl PyConfig {
    fn skippr_mut(&mut self) -> &mut Skippr {
        self.inner.skippr.get_or_insert_with(Skippr::default)
    }

    fn owns(slf: &Bound<'_, Self>, config: &Py<PyConfig>, what: &str) -> PyResult<()> {
        if config.is(slf) {
            return Ok(());
        }
        Err(PyValueError::new_err(format!(
            "{what} belongs to a different Config"
        )))
    }

    /// Merge `update` in with the same rules as `save`.
    fn merge(&mut self, update: &Config) -> PyResult<()> {
        self.inner = connect::merge(&self.inner, update).map_err(value_err)?;
        Ok(())
    }

    fn register_sink(
        slf: &Bound<'_, Self>,
        registry: SinkRegistry,
        name: &str,
        config: &Bound<'_, PyAny>,
        schema_sink: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        check_entry_name(name)?;
        let (entry, schema_name) = match extract_data_sink_config(config)? {
            SinkConfig::Paired(entry) => {
                let schema_name = match schema_sink {
                    None => sink_entries(registry, &slf.borrow().inner)
                        .and_then(|entries| entries.get(name))
                        .and_then(|existing| existing.schema_sink.as_deref())
                        .map(|reference| Config::parse_registry_ref(reference, Registry::SchemaSinks))
                        .transpose()
                        .map_err(value_err)?,
                    Some(value) => Some(value.extract::<String>().map_err(|_| {
                        PyTypeError::new_err(format!(
                            "{} shares its config with its schema sink; pass schema_sink as a name, not {}",
                            entry.plugin_name,
                            type_name(value)
                        ))
                    })?),
                };
                if let Some(schema) = &schema_name {
                    check_entry_name(schema)?;
                }
                (entry, schema_name)
            }
            SinkConfig::Unpaired(entry) => {
                let schema_name = match schema_sink {
                    None => None,
                    Some(value) => {
                        let schema = value.cast::<PySchemaSinkRef>().map_err(|_| {
                            PyTypeError::new_err(format!(
                                "schema_sink for {} must be a skippr.SchemaSinkRef, got {}",
                                entry.plugin_name,
                                type_name(value)
                            ))
                        })?;
                        let schema = schema.get();
                        Self::owns(slf, &schema.config, "schema_sink")?;
                        Some(schema.name.clone())
                    }
                };
                (entry, schema_name)
            }
        };
        let entry = DataSinkEntry {
            config: entry,
            schema_sink: schema_name
                .as_ref()
                .map(|schema| Registry::SchemaSinks.reference(schema)),
        };
        let mut this = slf.borrow_mut();
        this.inner =
            connect::register_sink(&this.inner, registry, name, &entry).map_err(value_err)?;
        Ok(())
    }

    fn entry_exists(&self, registry: Registry, name: &str) -> bool {
        match registry {
            Registry::DataSources => self
                .inner
                .data_sources
                .as_ref()
                .is_some_and(|e| e.contains_key(name)),
            Registry::DataSinks => self
                .inner
                .data_sinks
                .as_ref()
                .is_some_and(|e| e.contains_key(name)),
            Registry::DeadletterSinks => self
                .inner
                .deadletter_sinks
                .as_ref()
                .is_some_and(|e| e.contains_key(name)),
            Registry::SchemaSinks => self
                .inner
                .schema_sinks
                .as_ref()
                .is_some_and(|e| e.contains_key(name)),
        }
    }

    fn lookup(slf: &Bound<'_, Self>, registry: Registry, name: &str) -> PyResult<Py<PyConfig>> {
        if !slf.borrow().entry_exists(registry, name) {
            return Err(PyKeyError::new_err(registry.reference(name)));
        }
        Ok(slf.clone().unbind())
    }
}

#[pymethods]
impl PyConfig {
    #[new]
    fn new() -> Self {
        Self {
            inner: Config::new(),
            origin: None,
        }
    }

    #[staticmethod]
    fn load(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        let inner = py
            .detach(|| Config::parse_unresolved(&path))
            .map_err(value_err)?;
        Ok(Self {
            inner,
            origin: Some(path),
        })
    }

    #[staticmethod]
    fn discover(py: Python<'_>) -> PyResult<Self> {
        let path = connect::discover_config_path();
        if !path.exists() {
            return Ok(Self {
                inner: Config::new(),
                origin: Some(path),
            });
        }
        Self::load(py, path)
    }

    #[getter]
    fn path(&self) -> Option<String> {
        self.origin.as_ref().map(|p| p.display().to_string())
    }

    #[pyo3(signature = (path = None))]
    fn save(&self, py: Python<'_>, path: Option<PathBuf>) -> PyResult<()> {
        let target = path.or_else(|| self.origin.clone()).ok_or_else(|| {
            PyValueError::new_err("Config was not loaded from a file; call save(path)")
        })?;
        let inner = self.inner.clone();
        py.detach(|| connect::save_config(&target, &inner))
            .map_err(value_err)
    }

    fn to_yaml(&self) -> PyResult<String> {
        self.inner.to_yaml_string().map_err(value_err)
    }

    fn workspace(slf: Bound<'_, Self>, value: String) -> PyResult<Bound<'_, Self>> {
        slf.borrow_mut().skippr_mut().workspace = Some(non_empty("workspace", value)?);
        Ok(slf)
    }

    fn tenant(slf: Bound<'_, Self>, value: String) -> PyResult<Bound<'_, Self>> {
        slf.borrow_mut().skippr_mut().tenant = Some(non_empty("tenant", value)?);
        Ok(slf)
    }

    fn wal_s3_bucket(slf: Bound<'_, Self>, value: String) -> PyResult<Bound<'_, Self>> {
        slf.borrow_mut().skippr_mut().wal_s3_bucket = Some(non_empty("wal_s3_bucket", value)?);
        Ok(slf)
    }

    fn storage<'py>(
        slf: Bound<'py, Self>,
        value: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, Self>> {
        let (mode, bucket) = if value.cast::<PyLocalStorage>().is_ok() {
            (ElStorageMode::Local, None)
        } else if let Ok(s3) = value.cast::<PyS3Storage>() {
            (ElStorageMode::S3, Some(s3.get().bucket.clone()))
        } else {
            return Err(PyTypeError::new_err(format!(
                "expected skippr.LocalStorage or skippr.S3Storage, got {}",
                type_name(value)
            )));
        };
        {
            let mut this = slf.borrow_mut();
            let skippr = this.skippr_mut();
            skippr.skipprd_el_storage_mode = Some(mode);
            skippr.skippr_s3_bucket = bucket;
        }
        Ok(slf)
    }

    fn store<'py>(slf: Bound<'py, Self>, value: &Bound<'py, PyAny>) -> PyResult<Bound<'py, Self>> {
        let store = if value.cast::<PySledStore>().is_ok() {
            SkipprStore {
                kind: SkipprStoreKind::Sled,
                name: None,
            }
        } else if let Ok(dynamo) = value.cast::<PyDynamoDbStore>() {
            SkipprStore {
                kind: SkipprStoreKind::DynamoDb,
                name: Some(dynamo.get().table.clone()),
            }
        } else if let Ok(tables) = value.cast::<PyCloudTablesStore>() {
            SkipprStore {
                kind: SkipprStoreKind::CloudTables,
                name: Some(tables.get().table.clone()),
            }
        } else {
            return Err(PyTypeError::new_err(format!(
                "expected skippr.SledStore, skippr.DynamoDbStore, or skippr.CloudTablesStore, got {}",
                type_name(value)
            )));
        };
        slf.borrow_mut().skippr_mut().store = Some(store);
        Ok(slf)
    }

    fn data_source(
        slf: &Bound<'_, Self>,
        name: String,
        config: &Bound<'_, PyAny>,
    ) -> PyResult<PyDataSourceRef> {
        check_entry_name(&name)?;
        let entry = extract_data_source_config(config)?;
        slf.borrow_mut().merge(&Config {
            data_sources: entries_update(&name, entry),
            ..Config::default()
        })?;
        Ok(PyDataSourceRef {
            config: slf.clone().unbind(),
            name,
        })
    }

    #[pyo3(signature = (name, config, *, schema_sink = None))]
    fn data_sink(
        slf: &Bound<'_, Self>,
        name: String,
        config: &Bound<'_, PyAny>,
        schema_sink: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyDataSinkRef> {
        Self::register_sink(slf, SinkRegistry::Data, &name, config, schema_sink)?;
        Ok(PyDataSinkRef {
            config: slf.clone().unbind(),
            name,
        })
    }

    #[pyo3(signature = (name, config, *, schema_sink = None))]
    fn deadletter_sink(
        slf: &Bound<'_, Self>,
        name: String,
        config: &Bound<'_, PyAny>,
        schema_sink: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyDeadletterSinkRef> {
        Self::register_sink(slf, SinkRegistry::Deadletter, &name, config, schema_sink)?;
        Ok(PyDeadletterSinkRef {
            config: slf.clone().unbind(),
            name,
        })
    }

    fn schema_sink(
        slf: &Bound<'_, Self>,
        name: String,
        config: &Bound<'_, PyAny>,
    ) -> PyResult<PySchemaSinkRef> {
        check_entry_name(&name)?;
        let entry = extract_schema_sink_config(config)?;
        slf.borrow_mut().merge(&Config {
            schema_sinks: entries_update(&name, entry),
            ..Config::default()
        })?;
        Ok(PySchemaSinkRef {
            config: slf.clone().unbind(),
            name,
        })
    }

    fn pipeline(
        slf: &Bound<'_, Self>,
        name: String,
        pipeline: PyRef<'_, PyPipeline>,
    ) -> PyResult<PyPipelineRef> {
        check_entry_name(&name)?;
        Self::owns(slf, &pipeline.data_source.config, "data_source")?;
        if let Some(sink) = &pipeline.data_sink {
            Self::owns(slf, &sink.config, "data_sink")?;
        }
        if let Some(sink) = &pipeline.deadletter_sink {
            Self::owns(slf, &sink.config, "deadletter_sink")?;
        }
        let value =
            serde_json::to_value(&*pipeline).map_err(|e| PyValueError::new_err(e.to_string()))?;
        let engine = serde_json::from_value(value).map_err(|e| {
            PyValueError::new_err(format!("pipeline '{name}' is not a valid pipeline: {e}"))
        })?;
        let update = Config {
            pipelines: BTreeMap::from([(name.clone(), engine)]),
            ..Config::default()
        };
        slf.borrow_mut().merge(&update)?;
        Ok(PyPipelineRef {
            config: slf.clone().unbind(),
            name,
        })
    }

    fn get_pipeline(slf: &Bound<'_, Self>, name: String) -> PyResult<PyPipelineRef> {
        if !slf.borrow().inner.pipelines.contains_key(&name) {
            return Err(PyKeyError::new_err(format!("pipelines.{name}")));
        }
        Ok(PyPipelineRef {
            config: slf.clone().unbind(),
            name,
        })
    }

    fn get_data_source(slf: &Bound<'_, Self>, name: String) -> PyResult<PyDataSourceRef> {
        let config = Self::lookup(slf, Registry::DataSources, &name)?;
        Ok(PyDataSourceRef { config, name })
    }

    fn get_data_sink(slf: &Bound<'_, Self>, name: String) -> PyResult<PyDataSinkRef> {
        let config = Self::lookup(slf, Registry::DataSinks, &name)?;
        Ok(PyDataSinkRef { config, name })
    }

    fn get_deadletter_sink(slf: &Bound<'_, Self>, name: String) -> PyResult<PyDeadletterSinkRef> {
        let config = Self::lookup(slf, Registry::DeadletterSinks, &name)?;
        Ok(PyDeadletterSinkRef { config, name })
    }

    fn get_schema_sink(slf: &Bound<'_, Self>, name: String) -> PyResult<PySchemaSinkRef> {
        let config = Self::lookup(slf, Registry::SchemaSinks, &name)?;
        Ok(PySchemaSinkRef { config, name })
    }

    fn __repr__(&self) -> String {
        match &self.origin {
            Some(path) => format!("Config(path={:?})", path.display().to_string()),
            None => "Config()".into(),
        }
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyConfig>()?;
    m.add_class::<PyEnvRef>()?;
    m.add_class::<PyLocalStorage>()?;
    m.add_class::<PyS3Storage>()?;
    m.add_class::<PySledStore>()?;
    m.add_class::<PyDynamoDbStore>()?;
    m.add_class::<PyCloudTablesStore>()?;
    m.add_class::<PyDataSourceRef>()?;
    m.add_class::<PyDataSinkRef>()?;
    m.add_class::<PyDeadletterSinkRef>()?;
    m.add_class::<PySchemaSinkRef>()?;
    m.add_class::<PyPipelineRef>()?;
    let py = m.py();
    m.add(
        "Storage",
        union_alias([
            py.get_type::<PyLocalStorage>().into_any(),
            py.get_type::<PyS3Storage>().into_any(),
        ])?,
    )?;
    m.add(
        "Store",
        union_alias([
            py.get_type::<PySledStore>().into_any(),
            py.get_type::<PyDynamoDbStore>().into_any(),
            py.get_type::<PyCloudTablesStore>().into_any(),
        ])?,
    )?;
    register_generated_classes(m)
}
