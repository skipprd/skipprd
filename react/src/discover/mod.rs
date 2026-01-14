pub mod stats;

// Minimal stubs so provider/catalog signatures that previously accepted ingest Metadata
// can remain compatible during the crate split. These types will be removed once
// catalog building is fully provider-driven end-to-end.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Metadata {}

