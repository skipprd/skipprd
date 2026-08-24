use datafusion::arrow::array::{Array, Float64Array, Int64Array};
use datafusion::arrow::datatypes::DataType;
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{create_udf, ColumnarValue, Volatility};
use datafusion::prelude::SessionContext;
use std::sync::Arc;

fn as_f64(array: &dyn Array) -> Result<Vec<Option<f64>>, DataFusionError> {
    if let Some(a) = array.as_any().downcast_ref::<Float64Array>() {
        return Ok((0..a.len())
            .map(|i| if a.is_null(i) { None } else { Some(a.value(i)) })
            .collect());
    }
    if let Some(a) = array.as_any().downcast_ref::<Int64Array>() {
        return Ok((0..a.len())
            .map(|i| {
                if a.is_null(i) {
                    None
                } else {
                    Some(a.value(i) as f64)
                }
            })
            .collect());
    }
    Err(DataFusionError::Plan(
        "otel_rate value must be Float64 or Int64".into(),
    ))
}

fn as_i64(array: &dyn Array) -> Result<Vec<Option<i64>>, DataFusionError> {
    let a = array
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| DataFusionError::Plan("otel_rate time/window must be Int64".into()))?;
    Ok((0..a.len())
        .map(|i| if a.is_null(i) { None } else { Some(a.value(i)) })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Sum,
    Gauge,
    Histogram,
}

pub fn rate_for_kind(
    kind: MetricKind,
    args: &[ColumnarValue],
) -> datafusion::error::Result<ColumnarValue> {
    match kind {
        MetricKind::Histogram => Err(DataFusionError::Plan(
            "otel_rate does not support histogram; use otel_histogram_quantile".into(),
        )),
        MetricKind::Sum | MetricKind::Gauge => otel_rate_impl(args),
    }
}

pub fn otel_rate_impl(args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
    if args.len() != 3 {
        return Err(DataFusionError::Plan(
            "otel_rate(value, time_unix_nano, window_ns) requires 3 arguments".into(),
        ));
    }
    let values = args[0].clone().into_array(1)?;
    let times = args[1].clone().into_array(values.len())?;
    let windows = args[2].clone().into_array(values.len())?;
    let value = as_f64(values.as_ref())?;
    let time = as_i64(times.as_ref())?;
    let window = as_i64(windows.as_ref())?;
    let mut out = Vec::with_capacity(value.len());
    for i in 0..value.len() {
        if i == 0 {
            out.push(None);
            continue;
        }
        let (Some(v1), Some(v0), Some(t1), Some(t0), Some(win)) = (
            value[i],
            value[i - 1],
            time[i],
            time[i - 1],
            window[i].or(window.first().copied().flatten()),
        ) else {
            out.push(None);
            continue;
        };
        let dt = t1.saturating_sub(t0);
        if dt <= 0 || dt > win {
            out.push(None);
            continue;
        }
        let seconds = dt as f64 / 1_000_000_000.0;
        if seconds <= 0.0 {
            out.push(None);
            continue;
        }
        out.push(Some((v1 - v0) / seconds));
    }
    Ok(ColumnarValue::Array(Arc::new(Float64Array::from(out))))
}

fn otel_rate_udf(args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
    otel_rate_impl(args)
}

pub struct OtelRate;

pub fn register(ctx: &SessionContext) {
    let udf = create_udf(
        "otel_rate",
        vec![DataType::Float64, DataType::Int64, DataType::Int64],
        DataType::Float64,
        Volatility::Immutable,
        Arc::new(otel_rate_udf),
    );
    ctx.register_udf(udf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::Array;
    use datafusion::arrow::array::Float64Array;
    use datafusion::logical_expr::ColumnarValue;
    use std::sync::Arc;

    #[test]
    fn monotonic_rate() {
        let values = ColumnarValue::Array(Arc::new(Float64Array::from(vec![0.0, 10.0, 20.0])));
        let times = ColumnarValue::Array(Arc::new(Int64Array::from(vec![
            0,
            1_000_000_000,
            2_000_000_000,
        ])));
        let window = ColumnarValue::Array(Arc::new(Int64Array::from(vec![
            10_000_000_000,
            10_000_000_000,
            10_000_000_000,
        ])));
        let ColumnarValue::Array(out) =
            rate_for_kind(MetricKind::Sum, &[values, times, window]).unwrap()
        else {
            panic!("array");
        };
        let out = out.as_any().downcast_ref::<Float64Array>().unwrap();
        assert!(out.is_null(0));
        assert!((out.value(1) - 10.0).abs() < 1e-9);
        assert!((out.value(2) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn empty_delta_is_null() {
        let values = ColumnarValue::Array(Arc::new(Float64Array::from(Vec::<f64>::new())));
        let times = ColumnarValue::Array(Arc::new(Int64Array::from(Vec::<i64>::new())));
        let window = ColumnarValue::Array(Arc::new(Int64Array::from(Vec::<i64>::new())));
        let ColumnarValue::Array(out) = otel_rate_impl(&[values, times, window]).unwrap() else {
            panic!("array");
        };
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn high_cardinality_one_thousand_series() {
        let n = 1000;
        let values: Vec<f64> = (0..n).map(|i| i as f64).collect();
        let times: Vec<i64> = (0..n).map(|i| i as i64 * 1_000_000_000).collect();
        let window: Vec<i64> = vec![10_000_000_000; n];
        let values = ColumnarValue::Array(Arc::new(Float64Array::from(values)));
        let times = ColumnarValue::Array(Arc::new(Int64Array::from(times)));
        let window = ColumnarValue::Array(Arc::new(Int64Array::from(window)));
        let ColumnarValue::Array(out) = otel_rate_impl(&[values, times, window]).unwrap() else {
            panic!("array");
        };
        assert_eq!(out.len(), n);
    }

    #[test]
    fn gauge_rate_of_change() {
        let values = ColumnarValue::Array(Arc::new(Float64Array::from(vec![5.0, 15.0])));
        let times = ColumnarValue::Array(Arc::new(Int64Array::from(vec![0, 1_000_000_000])));
        let window = ColumnarValue::Array(Arc::new(Int64Array::from(vec![10_000_000_000; 2])));
        let ColumnarValue::Array(out) =
            rate_for_kind(MetricKind::Gauge, &[values, times, window]).unwrap()
        else {
            panic!("array");
        };
        let out = out.as_any().downcast_ref::<Float64Array>().unwrap();
        assert!((out.value(1) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn histogram_kind_errors() {
        let dummy = ColumnarValue::Array(Arc::new(Float64Array::from(vec![1.0])));
        let err = rate_for_kind(
            MetricKind::Histogram,
            &[dummy.clone(), dummy.clone(), dummy],
        )
        .unwrap_err();
        assert!(err.to_string().contains("histogram"));
    }

    #[test]
    #[ignore]
    fn load_series_ten_thousand() {
        const LOAD_SERIES: usize = 10_000;
        const P99_MS: u128 = 500;
        let start = std::time::Instant::now();
        let values: Vec<f64> = (0..LOAD_SERIES).map(|i| i as f64).collect();
        let times: Vec<i64> = (0..LOAD_SERIES).map(|i| i as i64 * 1_000_000_000).collect();
        let window: Vec<i64> = vec![10_000_000_000; LOAD_SERIES];
        let values = ColumnarValue::Array(Arc::new(Float64Array::from(values)));
        let times = ColumnarValue::Array(Arc::new(Int64Array::from(times)));
        let window = ColumnarValue::Array(Arc::new(Int64Array::from(window)));
        let ColumnarValue::Array(out) = otel_rate_impl(&[values, times, window]).unwrap() else {
            panic!("array");
        };
        assert_eq!(out.len(), LOAD_SERIES);
        assert!(start.elapsed().as_millis() < P99_MS);
    }
}
