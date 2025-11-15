use crate::catalog::model::FieldStatsLite;
use crate::discover::stats::FieldStats;

pub fn to_stats_lite(s: &FieldStats) -> FieldStatsLite {
    FieldStatsLite {
        total: s.total,
        nulls: s.nulls,
        min_numeric: s.min_numeric,
        max_numeric: s.max_numeric,
        min_len: s.min_len,
        max_len: s.max_len,
        approx_distinct: s.approx_distinct,
        histogram_bins: s.histogram_bins.clone(),
        histogram_min: s.histogram_min,
        histogram_max: s.histogram_max,
        last_updated_epoch_ms: s.last_updated_epoch_ms,
    }
}


