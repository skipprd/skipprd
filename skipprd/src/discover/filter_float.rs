





const FILTER_FLAG_ALLOW_THOUSAND: bool = false;


#[cfg(test)]
mod tests {
    
    
    
    use crate::discover::get_type;
    



    #[test]
    fn test_get_type_int() {
        let expected_type = "float".to_string();

        let subject = 123;
        assert_ne!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_bool_true() {
        let expected_type = "float".to_string();

        let subject = true;
        assert_ne!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_type_bool_false() {
        let expected_type = "float".to_string();

        let subject = false;
        assert_ne!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_type_bool_true_1() {
        let expected_type = "float".to_string();

        let subject = 1;
        assert_ne!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_type_bool_false_0() {
        let expected_type = "float".to_string();

        let subject = 0;
        assert_ne!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_type_string() {
        let expected_type = "float".to_string();

        let subject = "sd";
        assert_ne!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_type_float_1() {
        let expected_type = "float".to_string();

        let subject = 1.2;
        assert_eq!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_type_float_2() {
        let expected_type = "float".to_string();

        let subject = 0.2;
        assert_eq!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    // #[test]
    // fn test_get_type_float_3() {
    //     let expected_type = "float".to_string();
    //
    //     let subject = 2.0;
    //     assert_eq!(get_type(&mut String::from(subject.to_string())), expected_type);
    // }

    #[test]
    fn test_get_type_float_4() {
        let expected_type = "float".to_string();

        let subject = "0.0";
        assert_eq!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_type_float_5() {
        let expected_type = "float".to_string();

        let subject = "-0.1";
        assert_eq!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    #[test]
    fn test_get_type_float_6() {
        let expected_type = "float".to_string();

        let subject = "+0.1";
        assert_eq!(get_type(&mut String::from(subject.to_string())), expected_type);
    }

    // #[test]
    // fn test_get_type_float_7() {
    //     let expected_type = "float".to_string();
    //
    //     let subject = 0.0;
    //     assert_eq!(get_type(&mut String::from(subject.to_string())), expected_type);
    // }

}

pub fn parse_float(value: &mut String) -> Option<f64> {

        let len = value.len();
        // let mut len = value.chars().count();
        // let mut str = value.as_ptr() as usize;
        let mut str = 0;

        // let mut len = "ds".len();
        // let mut str = "ds".as_ptr() as usize;


        let end = str + len;

        // let mut decimal: *const c_char = ptr::null();
        let mut decimal_set = 0;
        let mut decimal_len = 0;
        // let mut dec_sep = '.' as c_char;

        // let mut thousand: *const c_char = ptr::null();
        let mut thousand_set = 0;
        let mut thousand_len = 0;
        // let mut tsd_sep: *const c_char = ptr::null();

        // let mut lval: zend_long = 0;
        // let mut dval: c_double = 0.0;
        // let mut min_range: c_double = 0.0;
        // let mut max_range: c_double = 0.0;
        // let mut min_range_set = 0;
        // let mut max_range_set = 0;

        let mut first = 0;
        let mut n = 0;

        let mut num = String::new();
        let mut p = 0;
        if str < end && (value.chars().nth(str) == Some('+') || value.chars().nth(str) == Some('-')) {
            num.push(value.chars().nth(str).unwrap());
            str += 1;
        }
        first = 1;
        loop {
            let mut n = 0;
            while str < end {
                let thischar = value.chars().nth(str);
                if thischar >= Some('0') && thischar <= Some('9') {
                    n += 1;
                    num.push(value.chars().nth(str).unwrap());
                }
                str += 1;

                if str == end || value.chars().nth(str) == Some('.') || value.chars().nth(str) == Some('e') || value.chars().nth(str) == Some('E') {
                    if first == end {
                        return None;
                    }
                    if value.chars().nth(str) == Some('.') {
                        num.push('.');
                        str += 1;
                        while str < end && value.chars().nth(str) >= Some('0') && value.chars().nth(str) <= Some('9') {
                            num.push(value.chars().nth(str).unwrap());
                            str += 1;
                        }
                    }
                    if value.chars().nth(str) == Some('e') || value.chars().nth(str) == Some('E') {
                        num.push(value.chars().nth(str).unwrap());
                        str += 1;
                        if str < end && (value.chars().nth(str) == Some('+') || value.chars().nth(str) == Some('-')) {
                            num.push(value.chars().nth(str).unwrap());
                            str += 1;
                        }
                        while str < end && value.chars().nth(str) >= Some('0') && value.chars().nth(str) <= Some('9') {
                            num.push(value.chars().nth(str).unwrap());
                            str += 1;
                        }
                    }
                    break;
                // }
                // if (FILTER_FLAG_ALLOW_THOUSAND) && ",".contains(value.chars().nth(str).unwrap()) {
                //     if first == 1 && (n < 1 || n > 3) || first != 1 && n != 3 {
                //         return None;
                //     }
                //     first = 0;
                //     str += 1;
                } else {
                    return None;
                }
            }

            if str == end {
                return Some(castToFloat(num))
                // return Some(num)
            }
        }
        if str != end {
            return None;
        }


    if num.len() > 0 {
        return Some(castToFloat(num))
    }

    return None

    // } else {
    //     return None;
    // }

}

fn castToFloat(num: String) -> f64 {

    return num.parse::<f64>().unwrap();

}