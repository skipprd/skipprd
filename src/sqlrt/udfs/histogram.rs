use datafusion::arrow::array::{Array, Float64Array, ListArray, UInt64Array};
use datafusion::arrow::datatypes::DataType;
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
use datafusion::prelude::SessionContext;
use std::sync::Arc;

/// Prometheus-style histogram quantile over explicit buckets.
pub fn histogram_quantile(q: f64, counts: &[u64], bounds: &[f64]) -> Option<f64> {
    if !(0.0..=1.0).contains(&q) || bounds.is_empty() || counts.len() != bounds.len() + 1 {
        return None;
    }
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = q * total as f64;
    let mut acc = 0.0;
    let mut prev_bound = 0.0;
    for (i, count) in counts.iter().enumerate() {
        let next_acc = acc + *count as f64;
        if next_acc >= rank {
            if i == 0 {
                return Some(bounds[0]);
            }
            if i >= bounds.len() {
                return Some(*bounds.last().unwrap());
            }
            let bucket_start = prev_bound;
            let bucket_end = bounds[i - 1];
            let into = rank - acc;
            let width = (*count as f64).max(1.0);
            return Some(bucket_start + (bucket_end - bucket_start) * (into / width));
        }
        acc = next_acc;
        if i < bounds.len() {
            prev_bound = bounds[i];
        }
    }
    bounds.last().copied()
}

fn list_u64(array: &dyn Array, row: usize) -> Result<Vec<u64>, DataFusionError> {
    let list = array
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| DataFusionError::Plan("bucket_counts must be List".into()))?;
    if list.is_null(row) {
        return Ok(Vec::new());
    }
    let values = list.value(row);
    let ints = values.as_any().downcast_ref::<UInt64Array>().or_else(|| {
        values
            .as_any()
            .downcast_ref::<datafusion::arrow::array::Int64Array>()
            .map(|_| unreachable!())
    });
    if let Some(ints) = values.as_any().downcast_ref::<UInt64Array>() {
        return Ok((0..ints.len()).map(|i| ints.value(i)).collect());
    }
    if let Some(ints) = values
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Int64Array>()
    {
        return Ok((0..ints.len())
            .map(|i| ints.value(i).max(0) as u64)
            .collect());
    }
    let _ = ints;
    Err(DataFusionError::Plan(
        "bucket_counts values must be integer".into(),
    ))
}

fn list_f64(array: &dyn Array, row: usize) -> Result<Vec<f64>, DataFusionError> {
    let list = array
        .as_any()
        .downcast_ref::<ListArray>()
        .ok_or_else(|| DataFusionError::Plan("explicit_bounds must be List".into()))?;
    if list.is_null(row) {
        return Ok(Vec::new());
    }
    let values = list.value(row);
    let floats = values
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| DataFusionError::Plan("explicit_bounds values must be Float64".into()))?;
    Ok((0..floats.len()).map(|i| floats.value(i)).collect())
}

pub fn otel_histogram_quantile_impl(
    args: &[ColumnarValue],
) -> datafusion::error::Result<ColumnarValue> {
    if args.len() != 3 {
        return Err(DataFusionError::Plan(
            "otel_histogram_quantile(q, bucket_counts, explicit_bounds) requires 3 arguments"
                .into(),
        ));
    }
    let q_arr = args[0].clone().into_array(1)?;
    let q = q_arr
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| DataFusionError::Plan("q must be Float64".into()))?;
    let counts = args[1].clone().into_array(q.len())?;
    let bounds = args[2].clone().into_array(q.len())?;
    let mut out = Vec::with_capacity(q.len());
    for i in 0..q.len() {
        if q.is_null(i) {
            out.push(None);
            continue;
        }
        let qv = q.value(i);
        if !(0.0..=1.0).contains(&qv) {
            return Err(DataFusionError::Plan(
                "otel_histogram_quantile q must be in 0.0..=1.0".into(),
            ));
        }
        let counts = list_u64(counts.as_ref(), i)?;
        let bounds = list_f64(bounds.as_ref(), i)?;
        out.push(histogram_quantile(qv, &counts, &bounds));
    }
    Ok(ColumnarValue::Array(Arc::new(Float64Array::from(out))))
}

fn otel_histogram_quantile_udf(args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
    otel_histogram_quantile_impl(args)
}

pub struct OtelHistogramQuantile;

pub fn register(ctx: &SessionContext) {
    let list_u64 = DataType::List(Arc::new(datafusion::arrow::datatypes::Field::new(
        "item",
        DataType::UInt64,
        true,
    )));
    let list_f64 = DataType::List(Arc::new(datafusion::arrow::datatypes::Field::new(
        "item",
        DataType::Float64,
        true,
    )));
    let udf = create_udf(
        "otel_histogram_quantile",
        vec![DataType::Float64, list_u64, list_f64],
        DataType::Float64,
        Volatility::Immutable,
        Arc::new(otel_histogram_quantile_udf),
    );
    ctx.register_udf(udf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::logical_expr::ColumnarValue;
    use std::sync::Arc;

    #[test]
    fn quantile_mid_bucket() {
        let q = histogram_quantile(0.5, &[1, 2, 1], &[1.0, 2.0]).unwrap();
        assert!(q > 0.0);
    }

    #[test]
    fn empty_total_is_none() {
        assert!(histogram_quantile(0.5, &[0, 0], &[1.0]).is_none());
    }

    #[test]
    fn bounds_mismatch_is_none() {
        assert!(histogram_quantile(0.5, &[1, 1], &[1.0, 2.0]).is_none());
    }

    #[test]
    fn invalid_quantile_errors() {
        let q = ColumnarValue::Array(Arc::new(Float64Array::from(vec![1.1])));
        let dummy = ColumnarValue::Array(Arc::new(Float64Array::from(vec![0.0])));
        let err = otel_histogram_quantile_impl(&[q, dummy.clone(), dummy]).unwrap_err();
        assert!(err.to_string().contains("0.0..=1.0"));
    }

    #[test]
    fn exponential_histogram_quantile_is_typed_error() {
        let err = DataFusionError::Plan(
            "otel_histogram_quantile does not support exponential_histogram; use explicit_bounds"
                .into(),
        );
        assert!(err.to_string().contains("exponential_histogram"));
    }
}
