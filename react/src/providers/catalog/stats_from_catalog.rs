use crate::discover::stats::{DatasetFieldStats, FieldStats};

pub fn dataset_field_stats_from_catalog_json(
    dataset_id: &str,
    val: &serde_json::Value,
) -> Option<DatasetFieldStats> {
    let fields = val.get("fields")?.as_array()?;
    let mut ns = DatasetFieldStats::new(dataset_id);
    for f in fields {
        let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if let Some(st) = f.get("stats").and_then(|x| x.as_object()) {
            let mut fs = FieldStats::default();
            fs.total = st.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
            fs.nulls = st.get("nulls").and_then(|x| x.as_u64()).unwrap_or(0);
            fs.min_numeric = st.get("min_numeric").and_then(|x| x.as_f64());
            fs.max_numeric = st.get("max_numeric").and_then(|x| x.as_f64());
            fs.min_len = st.get("min_len").and_then(|x| x.as_u64());
            fs.max_len = st.get("max_len").and_then(|x| x.as_u64());
            fs.approx_distinct = st.get("approx_distinct").and_then(|x| x.as_u64());
            fs.histogram_bins = st
                .get("histogram_bins")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_u64()).collect());
            fs.histogram_min = st.get("histogram_min").and_then(|x| x.as_f64());
            fs.histogram_max = st.get("histogram_max").and_then(|x| x.as_f64());
            fs.last_updated_epoch_ms = st
                .get("last_updated_epoch_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            ns.fields.insert(name.to_string(), fs);
        }
    }
    Some(ns)
}
