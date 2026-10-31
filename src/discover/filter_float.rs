#[allow(dead_code)]
const FILTER_FLAG_ALLOW_THOUSAND: bool = false;

#[cfg(test)]
mod tests {
    use crate::discover::get_type;
    use crate::discover::SkipprDataType;
    use serde_json::Value;

    #[test]
    fn test_get_type_int() {
        let subject = 123;
        assert_ne!(get_type(&mut subject.to_string()), SkipprDataType::Double);
    }

    #[test]
    fn test_get_bool_true() {
        let subject = true;
        assert_ne!(get_type(&mut subject.to_string()), SkipprDataType::Double);
    }

    #[test]
    fn test_get_type_bool_false() {
        let subject = false;
        assert_ne!(get_type(&mut subject.to_string()), SkipprDataType::Double);
    }

    #[test]
    fn test_get_type_bool_true_1() {
        let subject = 1;
        assert_ne!(get_type(&mut subject.to_string()), SkipprDataType::Double);
    }

    #[test]
    fn test_get_type_bool_false_0() {
        let subject = 0;
        assert_ne!(get_type(&mut subject.to_string()), SkipprDataType::Double);
    }

    #[test]
    fn test_get_type_string() {
        let subject = "sd";
        assert_ne!(get_type(&mut subject.to_string()), SkipprDataType::Double);
    }

    #[test]
    fn test_get_type_float_1() {
        let subject = 1.2;
        assert_eq!(get_type(&mut subject.to_string()), SkipprDataType::Decimal);
    }

    #[test]
    fn test_get_type_float_2() {
        let subject = 0.2;
        assert_eq!(get_type(&mut subject.to_string()), SkipprDataType::Decimal);
    }

    #[allow(dead_code)]
    fn test_get_type_float_3() {
        let subject = 2.0;
        assert_eq!(
            get_type(&mut String::from(subject.to_string())),
            SkipprDataType::Double
        );
    }

    #[test]
    fn test_get_type_float_4() {
        let subject = "0.0";
        assert_eq!(get_type(&mut subject.to_string()), SkipprDataType::Decimal);
    }

    #[test]
    fn test_get_type_float_4_2() {
        let subject = "23.4";
        let json_value: Value = serde_json::from_str(subject).unwrap();
        let value: &mut String = &mut json_value.to_string();
        assert_eq!(get_type(value), SkipprDataType::Decimal);
    }

    #[test]
    fn test_get_type_float_5() {
        let subject = "-0.1";
        assert_eq!(get_type(&mut subject.to_string()), SkipprDataType::Decimal);
    }

    #[test]
    fn test_get_type_float_6() {
        let subject = "+0.1";
        assert_eq!(get_type(&mut subject.to_string()), SkipprDataType::Decimal);
    }

    #[test]
    fn test_get_type_money_string_decimal() {
        assert_eq!(
            get_type(&mut "\"50.00\"".to_string()),
            SkipprDataType::Decimal
        );
        assert_eq!(
            get_type(&mut "\"120.50\"".to_string()),
            SkipprDataType::Decimal
        );
    }

    #[test]
    fn test_get_type_scientific_stays_double() {
        let subject = "1e3";
        assert_eq!(get_type(&mut subject.to_string()), SkipprDataType::Double);
    }

    #[test]
    fn test_get_type_version_like_not_decimal() {
        assert_eq!(
            get_type(&mut "\"1.0.0\"".to_string()),
            SkipprDataType::String
        );
    }
}

#[allow(dead_code)]
pub fn parse_float(value: &mut String) -> Option<f64> {
    let len = value.len();
    let mut str = 0;
    let end = str + len;

    let _decimal_set = 0;
    let _decimal_len = 0;
    let _thousand_set = 0;
    let _thousand_len = 0;
    let mut _first = 0;
    let _n = 0;

    let mut num = String::new();
    let _p = 0;

    // Handle sign
    if str < end && (value.chars().nth(str) == Some('+') || value.chars().nth(str) == Some('-')) {
        num.push(value.chars().nth(str).unwrap());
        str += 1;
    }

    _first = 1;
    let mut _n = 0;

    // Process digits before decimal point
    while str < end {
        let thischar = value.chars().nth(str);
        if thischar >= Some('0') && thischar <= Some('9') {
            _n += 1;
            num.push(value.chars().nth(str).unwrap());
        } else if thischar == Some('.') || thischar == Some('e') || thischar == Some('E') {
            break;
        } else {
            return None;
        }
        str += 1;
    }

    // Handle early exit case
    if _first == end {
        return None;
    }

    // Process decimal point and decimal digits
    if str < end && value.chars().nth(str) == Some('.') {
        num.push('.');
        str += 1;
        while str < end
            && value.chars().nth(str) >= Some('0')
            && value.chars().nth(str) <= Some('9')
        {
            num.push(value.chars().nth(str).unwrap());
            str += 1;
        }
    }

    // Process exponent
    if str < end && (value.chars().nth(str) == Some('e') || value.chars().nth(str) == Some('E')) {
        num.push(value.chars().nth(str).unwrap());
        str += 1;

        // Handle exponent sign
        if str < end && (value.chars().nth(str) == Some('+') || value.chars().nth(str) == Some('-'))
        {
            num.push(value.chars().nth(str).unwrap());
            str += 1;
        }

        // Process exponent digits
        let mut has_exp_digits = false;
        while str < end
            && value.chars().nth(str) >= Some('0')
            && value.chars().nth(str) <= Some('9')
        {
            num.push(value.chars().nth(str).unwrap());
            has_exp_digits = true;
            str += 1;
        }

        // Exponent must have at least one digit
        if !has_exp_digits {
            return None;
        }
    }

    // Make sure we consumed all input, otherwise it's not a valid float
    if str == end {
        return Some(cast_to_float(num));
    }

    None
}

#[allow(dead_code)]
fn cast_to_float(num: String) -> f64 {
    num.parse::<f64>().unwrap()
}
