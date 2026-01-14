#[allow(dead_code)]
pub const MAX_LENGTH_OF_LONG: u32 = 20;

#[allow(dead_code)]
pub fn php_filter_parse_int(str: String, ret: &mut i64) -> bool {
    let mut _ctx_value: i64 = 0;
    let mut sign: bool = false;
    let mut _digit: i32 = 0;

    let str_len = str.len();
    let mut n = 0;
    let end = str_len;

    match str.chars().nth(n) {
        Some('-') => {
            sign = true;
            // ZEND_FALLTHROUGH;
            n += 1;
        }
        Some('+') => {
            n += 1;
        }
        _ => {}
    }

    if str.chars().nth(n) == Some('0') && n + 1 == end {
        /* Special cases: +0 and -0 */
        *ret = cast_char_to_int(str.chars().nth(n).unwrap());
        return true;
    }

    /* must start with 1..9*/
    let current_char = str.chars().nth(n);
    if n < end && current_char >= Some('1') && current_char <= Some('9') {
        let signed_unsigned_int: i64;
        if sign {
            signed_unsigned_int = -1;
        } else {
            signed_unsigned_int = 1;
        }
        _ctx_value = signed_unsigned_int * cast_char_to_int(str.chars().nth(n).unwrap());

        n += 1;
    } else {
        return false;
    }

    if end > MAX_LENGTH_OF_LONG as usize
    /* number too long */
    // || (std::mem::size_of::<i64>() == 4 && (end as usize - str.as_ptr() as usize == MAX_LENGTH_OF_LONG - 1) && str.as_ptr().read() > b'2')
    {
        /* overflow */
        return false;
    }

    while n < end {
        if str.chars().nth(n) >= Some('0') && str.chars().nth(n) <= Some('9') {
            _digit = cast_char_to_int(str.chars().nth(n).unwrap()) as i32;
            n += 1;
            if (!sign) && _ctx_value <= (std::i64::MAX - _digit as i64) / 10 {
                _ctx_value = (_ctx_value * 10) + _digit as i64;
            } else if sign && _ctx_value >= (std::i64::MIN + _digit as i64) / 10 {
                _ctx_value = (_ctx_value * 10) - _digit as i64;
            } else {
                return false;
            }
        } else {
            return false;
        }
    }

    *ret = _ctx_value;
    true
}

#[allow(dead_code)]
fn cast_char_to_int(num: char) -> i64 {
    num.to_string().parse::<i64>().unwrap()
}

#[allow(dead_code)]
fn cast_string_to_int(num: String) -> i64 {
    num.parse::<i64>().unwrap()
}

#[test]
fn test_php_filter_parse_int() {
    let mut ret: i64 = 0;
    assert!(php_filter_parse_int("123".to_string(), &mut ret));
    assert_eq!(ret, 123);
    assert!(php_filter_parse_int("-123".to_string(), &mut ret));
    assert_eq!(ret, -123);
    assert!(php_filter_parse_int("+123".to_string(), &mut ret));
    assert_eq!(ret, 123);
    assert!(php_filter_parse_int("+0".to_string(), &mut ret));
    assert_eq!(ret, 0);
    assert!(php_filter_parse_int("-0".to_string(), &mut ret));
    assert_eq!(ret, 0);
    assert!(php_filter_parse_int("0".to_string(), &mut ret));
    assert_eq!(ret, 0);
    assert!(php_filter_parse_int("2147483647".to_string(), &mut ret));
    assert_eq!(ret, 2147483647);
    assert!(php_filter_parse_int("2147483648".to_string(), &mut ret));
    assert_eq!(ret, 2147483648);
    assert!(php_filter_parse_int("-2147483648".to_string(), &mut ret));
    assert_eq!(ret, -2147483648);
    assert!(php_filter_parse_int("-2147483649".to_string(), &mut ret));
    assert_eq!(ret, -2147483649);
    assert!(php_filter_parse_int(
        "9223372036854775807".to_string(),
        &mut ret
    ));
    assert_eq!(ret, 9223372036854775807);
    assert!(!php_filter_parse_int(
        "9223372036854775808".to_string(),
        &mut ret
    ));
    assert_eq!(ret, 9223372036854775807);
    assert!(php_filter_parse_int(
        "-9223372036854775807".to_string(),
        &mut ret
    ));
    assert_eq!(ret, -9223372036854775807);
    assert!(php_filter_parse_int(
        "-9223372036854775808".to_string(),
        &mut ret
    ));
    assert_eq!(ret, -9223372036854775808);
    assert!(!php_filter_parse_int(
        "-9223372036854775809".to_string(),
        &mut ret
    ));
    assert_eq!(ret, -9223372036854775808);

    use serde_json::Value;

    let data = r#"
        {
            "name": "John Doe",
            "age": 43,
            "phones": [
                "+44 1234567",
                "+44 2345678"
            ]
        }"#;

    // Parse the string of data into serde_json::Value.
    let v: Value = serde_json::from_str(data).unwrap();

    assert!(php_filter_parse_int(v["age"].to_string(), &mut ret));
    assert_eq!(ret, 43);
    // print!("Return is {}", ret);
    assert!(!php_filter_parse_int(v["phones"][0].to_string(), &mut ret));
    assert!(!php_filter_parse_int(v["phones"].to_string(), &mut ret));
    // print!("Return is {}", ret);
    // assert_eq!(ret, "+44 1234567");
    assert!(!php_filter_parse_int(v["name"].to_string(), &mut ret));
    // print!("Return is {}", ret);
    // assert_eq!(ret, "John Doe");
}
