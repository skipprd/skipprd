//! PyO3 `skipprd` module. `Session` is the same engine as the CLI.

use std::sync::OnceLock;

use ::skipprd::api::Session as EngineSession;
use ::skipprd::helpers::configuration::Config;
use arrow::array::RecordBatch;
use arrow::compute::concat_batches;
use arrow::pyarrow::ToPyArrow;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyModule, PyString};

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

#[pyclass(name = "Session", module = "skipprd")]
struct PySession {
    inner: EngineSession,
}

#[pymethods]
impl PySession {
    #[new]
    #[pyo3(signature = (config = None, pipeline = None, **kwargs))]
    fn new(
        py: Python<'_>,
        config: Option<Bound<'_, PyAny>>,
        pipeline: Option<String>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Self> {
        let inner = match config {
            Some(cfg) if cfg.is_instance_of::<PyString>() => {
                let path: String = cfg.extract()?;
                EngineSession::from_yml(path, pipeline.as_deref()).map_err(PyValueError::new_err)?
            }
            Some(cfg) => {
                let value = py_to_value(&cfg)?;
                let parsed: Config = serde_json::from_value(value)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
                EngineSession::from_config(parsed, pipeline.as_deref())
                    .map_err(PyValueError::new_err)?
            }
            None => {
                let kwargs = kwargs.ok_or_else(|| {
                    PyValueError::new_err(
                        "Session requires config= path/dict or skippr.yml fields as kwargs",
                    )
                })?;
                if kwargs.is_empty() {
                    return Err(PyValueError::new_err(
                        "Session requires config= path/dict or skippr.yml fields as kwargs",
                    ));
                }
                let value = py_to_value(kwargs.as_any())?;
                let parsed: Config = serde_json::from_value(value)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
                EngineSession::from_config(parsed, pipeline.as_deref())
                    .map_err(PyValueError::new_err)?
            }
        };
        let _ = py;
        Ok(Self { inner })
    }

    #[getter]
    fn pipeline(&self) -> Option<String> {
        self.inner.pipeline.clone()
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

#[pymodule]
fn skipprd(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySession>()?;
    Ok(())
}
