use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FieldStats {
    pub total: u64,
    pub nulls: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_total: Option<u64>,
    pub nullable_ratio: Option<f64>,
    pub min_numeric: Option<f64>,
    pub max_numeric: Option<f64>,
    pub min_len: Option<u64>,
    pub max_len: Option<u64>,
    pub approx_distinct: Option<u64>,
    pub last_updated_epoch_ms: u64,
    #[serde(skip)]
    hll_rho_max: u8,
    // Histogram for numeric fields (optional, computed at finalize)
    pub histogram_bins: Option<Vec<u64>>, // fixed-size bins (linear)
    pub histogram_min: Option<f64>,
    pub histogram_max: Option<f64>,
    #[serde(skip)]
    numeric_samples: Vec<f64>, // transient reservoir sample for histogram
    #[serde(skip)]
    examples: Vec<String>, // transient, small set of example scalar values for LLM context
}

impl FieldStats {
    pub fn update_value(&mut self, value: &serde_json::Value) {
        self.total = self.total.saturating_add(1);
        self.sample_total = Some(self.total);
        if value.is_null() {
            self.nulls = self.nulls.saturating_add(1);
            return;
        }
        match value {
            serde_json::Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    self.min_numeric = Some(self.min_numeric.map(|v| v.min(f)).unwrap_or(f));
                    self.max_numeric = Some(self.max_numeric.map(|v| v.max(f)).unwrap_or(f));
                    // Collect transient sample for histogram
                    Self::reservoir_push(
                        &mut self.numeric_samples,
                        self.total - self.nulls,
                        f,
                        2048,
                    );
                }
                self.observe_hll(&value);
                self.push_example(n.to_string());
            }
            serde_json::Value::String(s) => {
                let len = s.len() as u64;
                self.min_len = Some(self.min_len.map(|v| v.min(len)).unwrap_or(len));
                self.max_len = Some(self.max_len.map(|v| v.max(len)).unwrap_or(len));
                self.observe_hll(&value);
                self.push_example(s.clone());
            }
            serde_json::Value::Bool(b) => {
                self.observe_hll(&value);
                self.push_example(b.to_string());
            }
            _ => {}
        }
        self.last_updated_epoch_ms = current_millis();
    }

    fn observe_hll(&mut self, value: &serde_json::Value) {
        // Flajolet-Martin style: track max leading zeros among 64-bit hashes
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(value.to_string().as_bytes());
        let digest = hasher.finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest[0..8]);
        let x = u64::from_be_bytes(buf);
        let leading = x.leading_zeros() as u8; // 0..=64
        if leading > self.hll_rho_max {
            self.hll_rho_max = leading;
        }
    }

    pub fn finalize(&mut self) {
        // Compute nullable ratio
        let denom = self.total.max(1); // avoid div-by-zero
        self.nullable_ratio = Some((self.nulls as f64) / (denom as f64));
        // Only compute approx distinct if we observed any non-null values
        if self.total.saturating_sub(self.nulls) > 0 {
            // Estimate ~ 2^R / phi, phi≈0.77351; even R=0 yields ~1.29 → 1
            let r = self.hll_rho_max as f64;
            let estimate = (2f64.powf(r) / 0.77351f64).round() as u64;
            if estimate > 0 {
                self.approx_distinct = Some(estimate);
            }
        }
        // Histogram for numeric fields (if enabled via config)
        if crate::helpers::configuration::Config::stats_histogram_enabled() {
            let non_null = self.total.saturating_sub(self.nulls);
            if non_null > 0 && (!self.numeric_samples.is_empty()) {
                let min_v = self.min_numeric.unwrap_or_else(|| {
                    self.numeric_samples
                        .iter()
                        .cloned()
                        .fold(f64::INFINITY, f64::min)
                });
                let max_v = self.max_numeric.unwrap_or_else(|| {
                    self.numeric_samples
                        .iter()
                        .cloned()
                        .fold(f64::NEG_INFINITY, f64::max)
                });
                let bins = 20usize;
                let mut counts = vec![0u64; bins];
                if min_v == max_v {
                    counts[0] = non_null;
                } else {
                    let width = (max_v - min_v) / bins as f64;
                    for &v in &self.numeric_samples {
                        let mut idx = ((v - min_v) / width).floor() as isize;
                        if idx < 0 {
                            idx = 0;
                        }
                        if idx as usize >= bins {
                            idx = bins as isize - 1;
                        }
                        counts[idx as usize] = counts[idx as usize].saturating_add(1);
                    }
                    // Scale sample counts up to total non-null count
                    let sample_n = self.numeric_samples.len() as u64;
                    if sample_n > 0 {
                        let scale = (non_null as f64) / (sample_n as f64);
                        for c in counts.iter_mut() {
                            *c = ((*c as f64) * scale).round() as u64;
                        }
                    }
                }
                self.histogram_min = Some(min_v);
                self.histogram_max = Some(max_v);
                self.histogram_bins = Some(counts);
            }
        }
        // Drop samples to avoid any persistence (PII avoidance)
        self.numeric_samples.clear();
    }

    #[inline]
    pub fn examples(&self) -> &[String] {
        &self.examples
    }

    fn push_example(&mut self, val: String) {
        // Keep a small, unique set of short examples in-memory only
        if self.examples.len() >= 8 {
            return;
        }
        let mut v = val;
        if v.len() > 64 {
            v.truncate(64);
        }
        if !self.examples.iter().any(|e| e == &v) {
            self.examples.push(v);
        }
    }

    #[inline]
    fn reservoir_push(buf: &mut Vec<f64>, seen_non_null: u64, val: f64, capacity: usize) {
        if buf.len() < capacity {
            buf.push(val);
            return;
        }
        // Deterministic pseudo-random replacement based on seen count
        let idx = ((seen_non_null as usize)
            .wrapping_mul(1103515245)
            .wrapping_add(12345))
            % capacity;
        if capacity > 0 {
            buf[idx] = val;
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NamespaceStats {
    pub namespace: String,
    pub fields: HashMap<String, FieldStats>,
    pub last_updated_epoch_ms: u64,
}

impl NamespaceStats {
    pub fn new(namespace: &str) -> Self {
        Self {
            namespace: namespace.to_string(),
            fields: HashMap::new(),
            last_updated_epoch_ms: current_millis(),
        }
    }

    pub fn update_field(&mut self, field: &str, value: &serde_json::Value) {
        let entry = self
            .fields
            .entry(field.to_string())
            .or_insert_with(FieldStats::default);
        entry.update_value(value);
        self.last_updated_epoch_ms = current_millis();
    }
}

fn current_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn updates_numeric_and_string_bounds() {
        let mut ns = NamespaceStats::new("ns1");
        ns.update_field("a", &json!(3));
        ns.update_field("a", &json!(1));
        ns.update_field("a", &json!(5));
        ns.update_field("s", &json!("hi"));
        ns.update_field("s", &json!("hello"));
        let a = ns.fields.get("a").unwrap();
        assert_eq!(a.total, 3);
        assert_eq!(a.nulls, 0);
        assert_eq!(a.min_numeric, Some(1.0));
        assert_eq!(a.max_numeric, Some(5.0));
        let s = ns.fields.get("s").unwrap();
        assert_eq!(s.min_len, Some(2));
        assert_eq!(s.max_len, Some(5));
    }

    #[test]
    fn approx_distinct_estimation_present() {
        let mut ns = NamespaceStats::new("ns1");
        for i in 0..100 {
            ns.update_field("k", &json!(format!("val{}", i)));
        }
        if let Some(fs) = ns.fields.get_mut("k") {
            fs.finalize();
            assert!(fs.approx_distinct.unwrap_or(0) > 0);
        } else {
            panic!("missing field stats");
        }
    }

    #[test]
    fn handles_nulls_and_no_minmax_when_only_nulls() {
        let mut ns = NamespaceStats::new("ns1");
        ns.update_field("n", &json!(null));
        ns.update_field("n", &json!(null));
        let f = ns.fields.get("n").unwrap();
        assert_eq!(f.total, 2);
        assert_eq!(f.nulls, 2);
        assert!(f.min_numeric.is_none());
        assert!(f.max_numeric.is_none());
        assert!(f.min_len.is_none());
        assert!(f.max_len.is_none());
        let prev = f.last_updated_epoch_ms;
        // updating with another null still moves last_updated
        let mut f2 = f.clone();
        f2.update_value(&json!(null));
        assert!(f2.last_updated_epoch_ms >= prev);
        // no approx when only nulls
        let mut f3 = f2.clone();
        f3.finalize();
        assert!(f3.approx_distinct.is_none());
    }

    #[test]
    fn ignores_arrays_and_objects_for_bounds() {
        let mut ns = NamespaceStats::new("ns1");
        ns.update_field("x", &json!([1, 2, 3]));
        ns.update_field("x", &json!({"a":1}));
        let f = ns.fields.get("x").unwrap();
        assert_eq!(f.total, 2);
        assert_eq!(f.nulls, 0);
        assert!(f.min_numeric.is_none());
        assert!(f.max_numeric.is_none());
        assert!(f.min_len.is_none());
        assert!(f.max_len.is_none());
    }

    #[test]
    fn bool_values_contribute_to_distinct_only() {
        let mut ns = NamespaceStats::new("ns1");
        ns.update_field("b", &json!(true));
        ns.update_field("b", &json!(false));
        let mut f = ns.fields.get("b").unwrap().clone();
        assert_eq!(f.total, 2);
        assert_eq!(f.nulls, 0);
        assert!(f.min_numeric.is_none());
        assert!(f.max_numeric.is_none());
        assert!(f.min_len.is_none());
        assert!(f.max_len.is_none());
        f.finalize();
        assert!(f.approx_distinct.unwrap_or(0) >= 1);
    }

    #[test]
    fn numeric_histogram_is_generated() {
        crate::helpers::configuration::Config::set_evncache("STATS_HISTOGRAM_ENABLED", "true");
        let mut ns = NamespaceStats::new("ns1");
        for i in 0..1000 {
            ns.update_field("x", &json!(i as f64));
        }
        let mut f = ns.fields.get("x").unwrap().clone();
        f.finalize();
        assert!(f.histogram_bins.is_some());
        let bins = f.histogram_bins.unwrap();
        assert_eq!(bins.len(), 20);
        assert!(f.histogram_min.is_some());
        assert!(f.histogram_max.is_some());
    }
}
