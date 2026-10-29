use crate::discover::{AnalyseSchema, SkipprDataType};
use serde_json::Value;

/// Shared scalar coerce for slow and fast ingest. Evolution policy stays at the
/// call site: Create (slow `evolve_field`), ApplyOnly (fast existing siblings),
/// or Forbidden (primitive array elements).
pub(crate) fn coerce_scalar(
    data_type: &SkipprDataType,
    value: &Value,
) -> Result<Value, ScalarCoerceError> {
    if value.is_null() {
        return Ok(Value::Null);
    }

    match data_type {
        SkipprDataType::String => coerce_string(value),
        SkipprDataType::Integer => coerce_integer(value),
        SkipprDataType::Short => coerce_fit_i64(value, i16::MIN as i64, i16::MAX as i64),
        SkipprDataType::Byte => coerce_fit_i64(value, i8::MIN as i64, i8::MAX as i64),
        SkipprDataType::Long => coerce_long(value),
        SkipprDataType::Double | SkipprDataType::Float => coerce_double(value),
        SkipprDataType::Decimal => coerce_decimal(value),
        SkipprDataType::Boolean => coerce_boolean(value),
        SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => coerce_timestamp(value),
        SkipprDataType::Time => coerce_time(value),
        SkipprDataType::Uuid => coerce_uuid(value),
        SkipprDataType::Json => coerce_json(value),
        SkipprDataType::Binary | SkipprDataType::Fixed => coerce_stringish(value),
        SkipprDataType::Null => Err(ScalarCoerceError),
        SkipprDataType::Record
        | SkipprDataType::Map
        | SkipprDataType::Array
        | SkipprDataType::Date
        | SkipprDataType::Unknown => Err(ScalarCoerceError),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScalarCoerceError;

pub(crate) fn scalar_mismatch(
    data_type: &SkipprDataType,
    parent_field: Option<&str>,
    field: &str,
    value: &Value,
) -> String {
    format!(
        "Field: {} => {} value: {} is not a {}",
        parent_field.unwrap_or("root"),
        field,
        value,
        data_type.as_str()
    )
}

fn coerce_string(value: &Value) -> Result<Value, ScalarCoerceError> {
    if let Some(s) = value.as_str() {
        return Ok(Value::String(s.to_string()));
    }
    if value.is_number() {
        return Ok(Value::String(value.to_string()));
    }
    if let Some(b) = value.as_bool() {
        return Ok(Value::String(b.to_string()));
    }
    Err(ScalarCoerceError)
}

fn coerce_stringish(value: &Value) -> Result<Value, ScalarCoerceError> {
    coerce_string(value)
}

fn coerce_time(value: &Value) -> Result<Value, ScalarCoerceError> {
    let s = value.as_str().ok_or(ScalarCoerceError)?;
    if chrono::NaiveTime::parse_from_str(s, "%H:%M:%S").is_ok()
        || chrono::NaiveTime::parse_from_str(s, "%H:%M:%S%.f").is_ok()
    {
        return Ok(Value::String(s.to_string()));
    }
    Err(ScalarCoerceError)
}

fn coerce_uuid(value: &Value) -> Result<Value, ScalarCoerceError> {
    let s = value.as_str().ok_or(ScalarCoerceError)?;
    if uuid::Uuid::parse_str(s).is_ok() {
        Ok(Value::String(s.to_string()))
    } else {
        Err(ScalarCoerceError)
    }
}

fn coerce_json(value: &Value) -> Result<Value, ScalarCoerceError> {
    if value.is_object() || value.is_array() || value.is_number() || value.is_boolean() {
        Ok(value.clone())
    } else {
        Err(ScalarCoerceError)
    }
}

fn coerce_fit_i64(value: &Value, min: i64, max: i64) -> Result<Value, ScalarCoerceError> {
    let n = integer_as_i64(value)?;
    if n < min || n > max {
        return Err(ScalarCoerceError);
    }
    if max <= i32::MAX as i64 {
        Ok(Value::from(n as i32))
    } else {
        Ok(Value::from(n))
    }
}

fn integer_as_i64(value: &Value) -> Result<i64, ScalarCoerceError> {
    if let Some(i) = value.as_i64() {
        return Ok(i);
    }
    if let Some(s) = value.as_str() {
        if let Ok(v) = s.parse::<i64>() {
            return Ok(v);
        }
        if s == "true" {
            return Ok(1);
        }
        if s == "false" {
            return Ok(0);
        }
    }
    if let Some(b) = value.as_bool() {
        return Ok(if b { 1 } else { 0 });
    }
    if let Some(f) = value.as_f64() {
        if let Ok(v) = f.to_string().parse::<i64>() {
            return Ok(v);
        }
    }
    Err(ScalarCoerceError)
}

fn coerce_integer(value: &Value) -> Result<Value, ScalarCoerceError> {
    coerce_fit_i64(value, i32::MIN as i64, i32::MAX as i64)
}

fn coerce_long(value: &Value) -> Result<Value, ScalarCoerceError> {
    if let Some(i) = value.as_i64() {
        return Ok(Value::from(i));
    }
    if let Some(s) = value.as_str() {
        if let Ok(v) = s.parse::<i64>() {
            return Ok(Value::from(v));
        }
    }
    if let Some(f) = value.as_f64() {
        if let Ok(v) = f.to_string().parse::<i64>() {
            return Ok(Value::from(v));
        }
    }
    if let Some(b) = value.as_bool() {
        return Ok(Value::from(if b { 1i64 } else { 0i64 }));
    }
    Err(ScalarCoerceError)
}

fn coerce_double(value: &Value) -> Result<Value, ScalarCoerceError> {
    if let Some(f) = value.as_f64() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Ok(Value::Number(n));
        }
    }
    if let Some(s) = value.as_str() {
        if let Ok(f) = s.parse::<f64>() {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return Ok(Value::Number(n));
            }
        }
    }
    if let Some(i) = value.as_i64() {
        if let Some(n) = serde_json::Number::from_f64(i as f64) {
            return Ok(Value::Number(n));
        }
    }
    Err(ScalarCoerceError)
}

fn coerce_decimal(value: &Value) -> Result<Value, ScalarCoerceError> {
    if let Some(s) = value.as_str() {
        if s.parse::<f64>().is_ok() {
            return Ok(Value::String(s.to_string()));
        }
        return Err(ScalarCoerceError);
    }
    if value.as_f64().is_some() || value.as_i64().is_some() {
        return Ok(Value::String(value.to_string()));
    }
    Err(ScalarCoerceError)
}

fn coerce_boolean(value: &Value) -> Result<Value, ScalarCoerceError> {
    if let Some(b) = value.as_bool() {
        return Ok(Value::Bool(b));
    }
    if let Some(s) = value.as_str() {
        if let Ok(b) = s.parse::<bool>() {
            return Ok(Value::Bool(b));
        }
        if s == "0" {
            return Ok(Value::Bool(false));
        }
        if s == "1" {
            return Ok(Value::Bool(true));
        }
    }
    if let Some(i) = value.as_i64() {
        if i == 0 || i == 1 {
            return Ok(Value::Bool(i == 1));
        }
    }
    Err(ScalarCoerceError)
}

fn coerce_timestamp(value: &Value) -> Result<Value, ScalarCoerceError> {
    if let Some(i) = value.as_i64() {
        return Ok(AnalyseSchema::coerce_to_milli_seconds(Value::from(i)));
    }
    if let Some(s) = value.as_str() {
        if let Ok(i) = s.parse::<i64>() {
            return Ok(AnalyseSchema::coerce_to_milli_seconds(Value::from(i)));
        }
        if chrono::DateTime::parse_from_rfc3339(s).is_ok()
            || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
            || chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").is_ok()
        {
            return Ok(Value::String(s.replace(' ', "T")));
        }
    }
    if let Some(b) = value.as_bool() {
        return Ok(Value::from(if b { 1i64 } else { 0i64 }));
    }
    Err(ScalarCoerceError)
}

/// Compile-fail when a new `SkipprDataType` is not classified as structural vs scalar.
pub(crate) fn skippr_data_type_is_scalar_or_structural(dt: SkipprDataType) {
    match dt {
        SkipprDataType::Record
        | SkipprDataType::Map
        | SkipprDataType::Array
        | SkipprDataType::Date => {}
        SkipprDataType::String
        | SkipprDataType::Long
        | SkipprDataType::Integer
        | SkipprDataType::Short
        | SkipprDataType::Byte
        | SkipprDataType::Double
        | SkipprDataType::Float
        | SkipprDataType::Decimal
        | SkipprDataType::Boolean
        | SkipprDataType::TimestampMilli
        | SkipprDataType::Timestamp
        | SkipprDataType::Time
        | SkipprDataType::Binary
        | SkipprDataType::Uuid
        | SkipprDataType::Fixed
        | SkipprDataType::Json
        | SkipprDataType::Null
        | SkipprDataType::Unknown => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decimal_numeric_string_coerces() {
        assert_eq!(
            coerce_scalar(&SkipprDataType::Decimal, &json!("20.00")).unwrap(),
            json!("20.00")
        );
    }

    #[test]
    fn decimal_20mm_does_not_coerce() {
        assert!(coerce_scalar(&SkipprDataType::Decimal, &json!("20mm")).is_err());
    }

    #[test]
    fn integer_bwl_does_not_coerce() {
        assert!(coerce_scalar(&SkipprDataType::Integer, &json!("BWL")).is_err());
    }

    #[test]
    fn integer_numeric_string_coerces() {
        assert_eq!(
            coerce_scalar(&SkipprDataType::Integer, &json!("1234")).unwrap(),
            json!(1234)
        );
    }

    #[test]
    fn byte_rejects_values_outside_i8() {
        assert!(coerce_scalar(&SkipprDataType::Byte, &json!(1234)).is_err());
        assert_eq!(
            coerce_scalar(&SkipprDataType::Byte, &json!(12)).unwrap(),
            json!(12)
        );
    }

    #[test]
    fn short_rejects_values_outside_i16() {
        assert!(coerce_scalar(&SkipprDataType::Short, &json!(40000)).is_err());
        assert_eq!(
            coerce_scalar(&SkipprDataType::Short, &json!(12)).unwrap(),
            json!(12)
        );
    }
}
