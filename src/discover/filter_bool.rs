#[cfg(test)]
mod tests {

    use crate::discover::get_type;

    #[test]
    fn test_get_type_int() {
        let expected_type = "boolean".to_string();

        let subject = 123;
        assert_ne!(
            get_type(&mut subject.to_string()),
            expected_type
        );
    }

    #[test]
    fn test_get_type_true_int() {
        let expected_type = "boolean".to_string();

        let subject = 1;
        assert_eq!(
            get_type(&mut subject.to_string()),
            expected_type
        );
    }

    #[test]
    fn test_get_type_false_int() {
        let expected_type = "boolean".to_string();

        let subject = 0;
        assert_eq!(
            get_type(&mut subject.to_string()),
            expected_type
        );
    }

    #[test]
    fn test_get_type_true_bool() {
        let expected_type = "boolean".to_string();

        let subject = true;
        assert_eq!(
            get_type(&mut subject.to_string()),
            expected_type
        );
    }

    #[test]
    fn test_get_type_false_bool() {
        let expected_type = "boolean".to_string();

        let subject = false;
        assert_eq!(
            get_type(&mut subject.to_string()),
            expected_type
        );
    }

    #[test]
    fn test_get_type_true_str() {
        let expected_type = "boolean".to_string();

        let subject = "true";
        assert_eq!(
            get_type(&mut subject.to_string()),
            expected_type
        );
    }

    #[test]
    fn test_get_type_false_str() {
        let expected_type = "boolean".to_string();

        let subject = "false";
        assert_eq!(
            get_type(&mut subject.to_string()),
            expected_type
        );
    }
}

pub fn parse_bool(value: &mut String) -> Result<bool, i32> {
    let len = value.chars().count();
    let str = value;
    let mut ret: i32 = -1;

    // PHP_FILTER_TRIM_DEFAULT_EX(str, len, 0);

    /* returns true for "1", "true", "on" and "yes"
     * returns false for "0", "false", "off", "no", and ""
     * null otherwise. */
    match len {
        0 => {
            ret = 0;
        }
        1 => {
            if str == "1" {
                ret = 1;
            } else if *str == "0" {
                ret = 0;
            } else {
                ret = -1;
            }
        }
        2 => {
            if str.to_lowercase() == "on" {
                ret = 1;
            } else if str.to_lowercase() == "no" {
                ret = 0;
            } else {
                ret = -1;
            }
        }
        3 => {
            if str.to_lowercase() == "yes" {
                ret = 1;
            } else if str.to_lowercase() == "off" {
                ret = 0;
            } else {
                ret = -1;
            }
        }
        4 => {
            if str.to_lowercase() == "true" {
                ret = 1;
            } else {
                ret = -1;
            }
        }
        5 => {
            if str.to_lowercase() == "false" {
                ret = 0;
            } else {
                ret = -1;
            }
        }
        _ => {
            ret = -1;
        }
    }

    if ret == -1 {
        Err(ret)
    } else {
        Ok(cast_to_bool(ret))
    }
}

fn cast_to_bool(num: i32) -> bool {
    num.to_string().parse::<bool>().is_ok()
}
