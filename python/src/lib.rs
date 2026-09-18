//! PyO3 `skippr` module. `Session` is the same engine as the CLI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use ::skipprd::api::Session as EngineSession;
use ::skipprd::connect::{self, ConnectPlugin};
use ::skipprd::helpers::configuration::Config;
use ::skipprd::helpers::wal_storage::{ElStorageMode, OffsetStoreKind};
use arrow::array::RecordBatch;
use arrow::compute::concat_batches;
use arrow::pyarrow::ToPyArrow;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyModule};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    let json = obj.py().import("json")?;
    let dumped: String = json.call_method1("dumps", (obj,))?.extract()?;
    serde_json::from_str(&dumped).map_err(|e| PyValueError::new_err(e.to_string()))
}

fn value_to_py(py: Python<'_>, value: serde_json::Value) -> PyResult<Py<PyAny>> {
    let json = py.import("json")?;
    let s = serde_json::to_string(&value).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    Ok(json.call_method1("loads", (s,))?.unbind())
}

fn batches_to_table(py: Python<'_>, batches: Vec<RecordBatch>) -> PyResult<Py<PyAny>> {
    let pa = py.import("pyarrow")?;
    let table_cls = pa.getattr("Table")?;
    if batches.is_empty() {
        let kwargs = PyDict::new(py);
        kwargs.set_item("names", Vec::<String>::new())?;
        return Ok(table_cls
            .call_method("from_arrays", (Vec::<Py<PyAny>>::new(),), Some(&kwargs))?
            .unbind());
    }
    let schema = batches[0].schema();
    let batch =
        concat_batches(&schema, &batches).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    let py_batch = batch.to_pyarrow(py)?;
    Ok(table_cls
        .call_method1("from_batches", (vec![py_batch],))?
        .unbind())
}

fn config_path(config: Option<String>) -> PathBuf {
    config
        .map(PathBuf::from)
        .unwrap_or_else(connect::discover_config_path)
}

#[pyclass(eq, eq_int, from_py_object, name = "StorageMode", module = "skippr")]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum PyStorageMode {
    #[pyo3(name = "LOCAL")]
    Local,
    #[pyo3(name = "S3")]
    S3,
}

impl From<PyStorageMode> for ElStorageMode {
    fn from(value: PyStorageMode) -> Self {
        match value {
            PyStorageMode::Local => Self::Local,
            PyStorageMode::S3 => Self::S3,
        }
    }
}

#[pyclass(eq, eq_int, from_py_object, name = "OffsetStore", module = "skippr")]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum PyOffsetStore {
    #[pyo3(name = "SLED")]
    Sled,
    #[pyo3(name = "DYNAMODB")]
    DynamoDb,
    #[pyo3(name = "CLOUD_TABLES")]
    CloudTables,
}

impl From<PyOffsetStore> for OffsetStoreKind {
    fn from(value: PyOffsetStore) -> Self {
        match value {
            PyOffsetStore::Sled => Self::Sled,
            PyOffsetStore::DynamoDb => Self::DynamoDb,
            PyOffsetStore::CloudTables => Self::CloudTables,
        }
    }
}

#[pyclass(name = "SkipprRoot", module = "skippr")]
struct PySkipprRoot {
    path: PathBuf,
}

impl PySkipprRoot {
    fn persist(
        &self,
        workspace: Option<&str>,
        storage_mode: Option<ElStorageMode>,
        wal_s3_bucket: Option<&str>,
        offset_store: Option<OffsetStoreKind>,
        offset_dynamodb_table: Option<&str>,
        skippr_s3_bucket: Option<&str>,
        tenant: Option<&str>,
    ) -> PyResult<()> {
        connect::persist_skippr_keys(
            &self.path,
            workspace,
            storage_mode,
            wal_s3_bucket,
            offset_store,
            offset_dynamodb_table,
            skippr_s3_bucket,
            tenant,
        )
        .map(|_| ())
        .map_err(PyValueError::new_err)
    }
}

#[pymethods]
impl PySkipprRoot {
    fn workspace(slf: Bound<'_, Self>, value: String) -> PyResult<Py<Self>> {
        slf.borrow()
            .persist(Some(&value), None, None, None, None, None, None)?;
        Ok(slf.unbind())
    }

    fn storage_mode(slf: Bound<'_, Self>, value: PyStorageMode) -> PyResult<Py<Self>> {
        slf.borrow()
            .persist(None, Some(value.into()), None, None, None, None, None)?;
        Ok(slf.unbind())
    }

    fn offset_store(slf: Bound<'_, Self>, value: PyOffsetStore) -> PyResult<Py<Self>> {
        slf.borrow()
            .persist(None, None, None, Some(value.into()), None, None, None)?;
        Ok(slf.unbind())
    }

    fn wal_s3_bucket(slf: Bound<'_, Self>, value: String) -> PyResult<Py<Self>> {
        slf.borrow()
            .persist(None, None, Some(&value), None, None, None, None)?;
        Ok(slf.unbind())
    }

    fn offset_dynamodb_table(slf: Bound<'_, Self>, value: String) -> PyResult<Py<Self>> {
        slf.borrow()
            .persist(None, None, None, None, Some(&value), None, None)?;
        Ok(slf.unbind())
    }

    fn skippr_s3_bucket(slf: Bound<'_, Self>, value: String) -> PyResult<Py<Self>> {
        slf.borrow()
            .persist(None, None, None, None, None, Some(&value), None)?;
        Ok(slf.unbind())
    }

    fn tenant(slf: Bound<'_, Self>, value: String) -> PyResult<Py<Self>> {
        slf.borrow()
            .persist(None, None, None, None, None, None, Some(&value))?;
        Ok(slf.unbind())
    }
}

fn skippr_root(config: Option<String>) -> PySkipprRoot {
    PySkipprRoot {
        path: config_path(config),
    }
}

#[pyfunction]
#[pyo3(signature = (value, config = None))]
fn workspace(py: Python<'_>, value: String, config: Option<String>) -> PyResult<Py<PySkipprRoot>> {
    let root = skippr_root(config);
    root.persist(Some(&value), None, None, None, None, None, None)?;
    Bound::new(py, root).map(|b| b.unbind())
}

#[pyfunction]
#[pyo3(signature = (value, config = None))]
fn storage_mode(
    py: Python<'_>,
    value: PyStorageMode,
    config: Option<String>,
) -> PyResult<Py<PySkipprRoot>> {
    let root = skippr_root(config);
    root.persist(None, Some(value.into()), None, None, None, None, None)?;
    Bound::new(py, root).map(|b| b.unbind())
}

#[pyfunction]
#[pyo3(signature = (value, config = None))]
fn offset_store(
    py: Python<'_>,
    value: PyOffsetStore,
    config: Option<String>,
) -> PyResult<Py<PySkipprRoot>> {
    let root = skippr_root(config);
    root.persist(None, None, None, Some(value.into()), None, None, None)?;
    Bound::new(py, root).map(|b| b.unbind())
}

#[pyfunction]
#[pyo3(signature = (value, config = None))]
fn wal_s3_bucket(
    py: Python<'_>,
    value: String,
    config: Option<String>,
) -> PyResult<Py<PySkipprRoot>> {
    let root = skippr_root(config);
    root.persist(None, None, Some(&value), None, None, None, None)?;
    Bound::new(py, root).map(|b| b.unbind())
}

#[pyfunction]
#[pyo3(signature = (value, config = None))]
fn offset_dynamodb_table(
    py: Python<'_>,
    value: String,
    config: Option<String>,
) -> PyResult<Py<PySkipprRoot>> {
    let root = skippr_root(config);
    root.persist(None, None, None, None, Some(&value), None, None)?;
    Bound::new(py, root).map(|b| b.unbind())
}

#[pyfunction]
#[pyo3(signature = (value, config = None))]
fn skippr_s3_bucket(
    py: Python<'_>,
    value: String,
    config: Option<String>,
) -> PyResult<Py<PySkipprRoot>> {
    let root = skippr_root(config);
    root.persist(None, None, None, None, None, Some(&value), None)?;
    Bound::new(py, root).map(|b| b.unbind())
}

#[pyfunction]
#[pyo3(signature = (value, config = None))]
fn tenant(py: Python<'_>, value: String, config: Option<String>) -> PyResult<Py<PySkipprRoot>> {
    let root = skippr_root(config);
    root.persist(None, None, None, None, None, None, Some(&value))?;
    Bound::new(py, root).map(|b| b.unbind())
}

#[pyclass(name = "Config", module = "skippr")]
#[derive(Clone)]
struct PyConfig {
    inner: Config,
}

impl PyConfig {
    fn skippr_mut(&mut self) -> &mut ::skipprd::helpers::configuration::Skippr {
        self.inner
            .skippr
            .get_or_insert_with(|| ::skipprd::helpers::configuration::Skippr {
                workspace: None,
                tenant: None,
                skippr_s3_bucket: None,
                skipprd_el_storage_mode: None,
                wal_s3_bucket: None,
                offset_store: None,
                offset_dynamodb_table: None,
            })
    }

    fn set_json_field<T: serde::de::DeserializeOwned>(
        &mut self,
        obj: &Bound<'_, PyAny>,
        write: impl FnOnce(&mut Config, T),
    ) -> PyResult<()> {
        let value = py_to_value(obj)?;
        let parsed =
            serde_json::from_value(value).map_err(|e| PyValueError::new_err(e.to_string()))?;
        write(&mut self.inner, parsed);
        Ok(())
    }
}

#[pymethods]
impl PyConfig {
    #[new]
    #[pyo3(signature = (**kwargs))]
    fn new(kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        let inner = match kwargs {
            Some(kwargs) if !kwargs.is_empty() => {
                let value = py_to_value(kwargs.as_any())?;
                serde_json::from_value(value).map_err(|e| PyValueError::new_err(e.to_string()))?
            }
            _ => Config::new(),
        };
        Ok(Self { inner })
    }

    fn workspace(slf: Bound<'_, Self>, value: String) -> Py<Self> {
        slf.borrow_mut().skippr_mut().workspace = Some(value);
        slf.unbind()
    }

    fn storage_mode(slf: Bound<'_, Self>, value: PyStorageMode) -> Py<Self> {
        slf.borrow_mut().skippr_mut().skipprd_el_storage_mode = Some(value.into());
        slf.unbind()
    }

    fn pipelines(slf: Bound<'_, Self>, value: Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        slf.borrow_mut()
            .set_json_field(&value, |cfg, parsed| cfg.pipelines = parsed)?;
        Ok(slf.unbind())
    }

    fn data_sources(slf: Bound<'_, Self>, value: Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        slf.borrow_mut()
            .set_json_field(&value, |cfg, parsed| cfg.data_sources = Some(parsed))?;
        Ok(slf.unbind())
    }

    fn data_sinks(slf: Bound<'_, Self>, value: Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        slf.borrow_mut()
            .set_json_field(&value, |cfg, parsed| cfg.data_sinks = Some(parsed))?;
        Ok(slf.unbind())
    }

    fn schema_sinks(slf: Bound<'_, Self>, value: Bound<'_, PyAny>) -> PyResult<Py<Self>> {
        slf.borrow_mut()
            .set_json_field(&value, |cfg, parsed| cfg.schema_sinks = Some(parsed))?;
        Ok(slf.unbind())
    }
}

#[pyclass(name = "Session", module = "skippr")]
struct PySession {
    inner: EngineSession,
}

#[pymethods]
impl PySession {
    #[new]
    #[pyo3(signature = (pipeline, config = None, *, config_file = None))]
    fn new(
        pipeline: String,
        config: Option<Bound<'_, PyConfig>>,
        config_file: Option<String>,
    ) -> PyResult<Self> {
        if pipeline.trim().is_empty() {
            return Err(PyValueError::new_err("Session requires pipeline="));
        }
        let inner = match (config, config_file) {
            (Some(_), Some(_)) => {
                return Err(PyValueError::new_err(
                    "Session accepts config= or config_file=, not both",
                ));
            }
            (Some(cfg), None) => {
                EngineSession::from_config(cfg.borrow().inner.clone(), Some(pipeline.as_str()))
                    .map_err(PyValueError::new_err)?
            }
            (None, Some(path)) => EngineSession::from_yml(path, Some(pipeline.as_str()))
                .map_err(PyValueError::new_err)?,
            (None, None) => EngineSession::from_discovered(Some(pipeline.as_str()))
                .map_err(PyValueError::new_err)?,
        };
        Ok(Self { inner })
    }

    fn config(slf: Bound<'_, Self>, config: Bound<'_, PyConfig>) -> Py<Self> {
        slf.borrow_mut()
            .inner
            .set_config(config.borrow().inner.clone());
        slf.unbind()
    }

    #[getter]
    fn pipeline(&self) -> Option<String> {
        self.inner.pipeline.clone()
    }

    fn connect(slf: Bound<'_, Self>) -> PyResult<Py<PyConnect>> {
        let session = slf.borrow();
        let pipeline = session
            .inner
            .require_pipeline()
            .map_err(PyValueError::new_err)?
            .to_string();
        let path = session.inner.connect_path();
        drop(session);
        Bound::new(
            slf.py(),
            PyConnect {
                session: slf.unbind(),
                path,
                pipeline,
                plugin: None,
                name: None,
                fields: BTreeMap::new(),
            },
        )
        .map(|b| b.unbind())
    }

    fn doctor(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let result = py.detach(|| runtime().block_on(self.inner.doctor()));
        let value =
            serde_json::to_value(&result).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        value_to_py(py, value)
    }

    fn discover(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            runtime()
                .block_on(self.inner.discover("text"))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }

    #[pyo3(signature = (once = false))]
    fn sync(&self, py: Python<'_>, once: bool) -> PyResult<()> {
        py.detach(|| {
            runtime()
                .block_on(self.inner.sync(once, "text"))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }

    fn query(&self, py: Python<'_>, sql: &str) -> PyResult<Py<PyAny>> {
        let sql = sql.to_string();
        let batches = py.detach(|| {
            runtime()
                .block_on(self.inner.query(&sql))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })?;
        batches_to_table(py, batches)
    }

    #[pyo3(signature = (name = None))]
    fn df(&self, py: Python<'_>, name: Option<&str>) -> PyResult<Py<PyAny>> {
        let name = name.map(str::to_string);
        let batches = py.detach(|| {
            runtime()
                .block_on(self.inner.df(name.as_deref()))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })?;
        batches_to_table(py, batches)
    }
}

#[pyclass(name = "Connect", module = "skippr")]
struct PyConnect {
    session: Py<PySession>,
    path: PathBuf,
    pipeline: String,
    plugin: Option<ConnectPlugin>,
    name: Option<String>,
    fields: BTreeMap<String, String>,
}

impl PyConnect {
    fn set_field(&mut self, ident: &str, value: String) -> PyResult<()> {
        self.fields.insert(ident.to_string(), value);
        self.maybe_persist()
    }

    fn maybe_persist(&mut self) -> PyResult<()> {
        let Some(plugin) = self.plugin else {
            return Ok(());
        };
        let Some(name) = self.name.as_deref() else {
            return Ok(());
        };
        if name.trim().is_empty() {
            return Ok(());
        }
        let yaml_fields =
            connect::yaml_path_fields(plugin, &connect::yaml_string_map(self.fields.clone()))
                .map_err(PyValueError::new_err)?;
        let required = plugin.required_fields();
        let secrets = plugin.secret_fields();
        let complete = required
            .iter()
            .all(|field| yaml_fields.contains_key(*field));
        let has_secret = secrets.iter().any(|field| yaml_fields.contains_key(*field));
        if !complete && !has_secret {
            return Ok(());
        }
        let doc = connect::persist_plugin(&self.path, &self.pipeline, plugin, name, yaml_fields)
            .map_err(PyValueError::new_err)?;
        Python::attach(|py| {
            let mut session = self.session.borrow_mut(py);
            session
                .inner
                .reload_from_document(self.path.clone(), doc)
                .map_err(PyValueError::new_err)
        })
    }
}

include!("connect_generated.rs");

#[pymodule]
fn skippr(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyConfig>()?;
    m.add_class::<PySession>()?;
    m.add_class::<PyConnect>()?;
    m.add_class::<PySkipprRoot>()?;
    m.add_class::<PyStorageMode>()?;
    m.add_class::<PyOffsetStore>()?;
    m.add_class::<PyDataSource>()?;
    m.add_class::<PyDataSink>()?;
    m.add_class::<PySchemaSink>()?;
    m.add_function(wrap_pyfunction!(workspace, m)?)?;
    m.add_function(wrap_pyfunction!(storage_mode, m)?)?;
    m.add_function(wrap_pyfunction!(offset_store, m)?)?;
    m.add_function(wrap_pyfunction!(wal_s3_bucket, m)?)?;
    m.add_function(wrap_pyfunction!(offset_dynamodb_table, m)?)?;
    m.add_function(wrap_pyfunction!(skippr_s3_bucket, m)?)?;
    m.add_function(wrap_pyfunction!(tenant, m)?)?;
    Ok(())
}
