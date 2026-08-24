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
        "otel_increase value must be Float64 or Int64".into(),
    ))
}

pub fn otel_increase_impl(args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
    if args.len() != 2 {
        return Err(DataFusionError::Plan(
            "otel_increase(value, time_unix_nano) requires 2 arguments".into(),
        ));
    }
    let values = args[0].clone().into_array(1)?;
    let value = as_f64(values.as_ref())?;
    let mut out = Vec::with_capacity(value.len());
    for i in 0..value.len() {
        if i == 0 {
            out.push(None);
            continue;
        }
        match (value[i], value[i - 1]) {
            (Some(v1), Some(v0)) if v1 >= v0 => out.push(Some(v1 - v0)),
            (Some(v1), Some(_v0)) => out.push(Some(v1)), // reset
            _ => out.push(None),
        }
    }
    Ok(ColumnarValue::Array(Arc::new(Float64Array::from(out))))
}

fn otel_increase_udf(args: &[ColumnarValue]) -> datafusion::error::Result<ColumnarValue> {
    otel_increase_impl(args)
}

pub struct OtelIncrease;

pub fn register(ctx: &SessionContext) {
    let udf = create_udf(
        "otel_increase",
        vec![DataType::Float64, DataType::Int64],
        DataType::Float64,
        Volatility::Immutable,
        Arc::new(otel_increase_udf),
    );
    ctx.register_udf(udf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::Array;

    #[test]
    fn monotonic_increase() {
        let values = ColumnarValue::Array(Arc::new(Float64Array::from(vec![1.0, 4.0, 4.0])));
        let times = ColumnarValue::Array(Arc::new(Int64Array::from(vec![0, 1, 2])));
        let ColumnarValue::Array(out) = otel_increase_impl(&[values, times]).unwrap() else {
            panic!("array");
        };
        let out = out.as_any().downcast_ref::<Float64Array>().unwrap();
        assert!(out.is_null(0));
        assert!((out.value(1) - 3.0).abs() < 1e-9);
        assert!((out.value(2) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn reset_uses_current_value() {
        let values = ColumnarValue::Array(Arc::new(Float64Array::from(vec![10.0, 2.0])));
        let times = ColumnarValue::Array(Arc::new(Int64Array::from(vec![0, 1])));
        let ColumnarValue::Array(out) = otel_increase_impl(&[values, times]).unwrap() else {
            panic!("array");
        };
        let out = out.as_any().downcast_ref::<Float64Array>().unwrap();
        assert!((out.value(1) - 2.0).abs() < 1e-9);
    }
}
