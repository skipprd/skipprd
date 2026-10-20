use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use std::path::PathBuf;

/// Author a `migrations/<id>.sql` file. The engine never loads Python.
#[pyclass(frozen, name = "Migration", module = "skippr")]
pub struct PyMigration {
    id: String,
    up: String,
    down: Option<String>,
}

#[pymethods]
impl PyMigration {
    #[new]
    #[pyo3(signature = (id, up, down=None))]
    fn new(id: String, up: String, down: Option<String>) -> PyResult<Self> {
        if id.trim().is_empty() {
            return Err(PyValueError::new_err("migration id must not be empty"));
        }
        if up.trim().is_empty() {
            return Err(PyValueError::new_err(
                "migration up() SQL must not be empty",
            ));
        }
        Ok(Self { id, up, down })
    }

    fn write(&self, dir: PathBuf) -> PyResult<PathBuf> {
        skipprd::migration::write_sql_file(&dir, &self.id, &self.up, self.down.as_deref())
            .map_err(PyValueError::new_err)
    }
}
