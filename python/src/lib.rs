//! PyO3 `skippr` module. `Session` is the same engine as the CLI.

mod config;

use std::sync::OnceLock;

use ::skipprd::api::Session as EngineSession;
use ::skipprd::helpers::configuration::Config;
use arrow::array::RecordBatch;
use arrow::compute::concat_batches;
use arrow::pyarrow::ToPyArrow;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyModule};

use config::PyPipelineRef;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
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

/// The engine bound to one pipeline of a `Config`, with `${ENV}` resolved.
#[pyclass(name = "Session", module = "skippr")]
struct PySession {
    inner: EngineSession,
    config: Config,
    pipeline: String,
}

impl PySession {
    /// The engine's startup checks, raised instead of exiting the interpreter.
    /// Run before every engine call. Process env settings such as `WAL_STORAGE`
    /// are cached on first read, so a change after that is not seen.
    fn admit(&self) -> PyResult<()> {
        let mut violations = self.config.get_config_dependency_violations();
        if let Err(err) = self.config.try_data_dir() {
            violations.push(err);
        }
        if violations.is_empty() {
            return Ok(());
        }
        Err(PyValueError::new_err(violations.join("\n")))
    }
}

#[pymethods]
impl PySession {
    #[new]
    fn new(py: Python<'_>, pipeline: PyRef<'_, PyPipelineRef>) -> PyResult<Self> {
        let name = pipeline.pipeline_name();
        let config = pipeline.resolved_config(py)?.bind_pipeline(name);
        let inner = EngineSession::from_config(config.clone(), Some(name));
        let session = Self {
            inner,
            config,
            pipeline: name.to_string(),
        };
        session.admit()?;
        Ok(session)
    }

    #[getter]
    fn pipeline(&self) -> String {
        self.pipeline.clone()
    }

    fn doctor(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.admit()?;
        let result = py.detach(|| runtime().block_on(self.inner.doctor()));
        let value =
            serde_json::to_value(&result).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        value_to_py(py, value)
    }

    fn discover(&self, py: Python<'_>) -> PyResult<()> {
        self.admit()?;
        py.detach(|| {
            runtime()
                .block_on(self.inner.discover("text"))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }

    #[pyo3(signature = (once = false))]
    fn sync(&self, py: Python<'_>, once: bool) -> PyResult<()> {
        self.admit()?;
        py.detach(|| {
            runtime()
                .block_on(self.inner.sync(once, "text"))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }

    fn query(&self, py: Python<'_>, sql: &str) -> PyResult<Py<PyAny>> {
        self.admit()?;
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
        self.admit()?;
        let name = name.map(str::to_string);
        let batches = py.detach(|| {
            runtime()
                .block_on(self.inner.df(name.as_deref()))
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })?;
        batches_to_table(py, batches)
    }
}

#[pymodule]
fn skippr(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySession>()?;
    config::register(m)
}
