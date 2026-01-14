pub fn parse_bool(value: &mut str) -> Result<bool, i32> {
    let len = value.chars().count();
    let str = value;
    let mut _ret: i32 = -1;

    // PHP_FILTER_TRIM_DEFAULT_EX(str, len, 0);

    /* returns true for "1", "true", "on" and "yes"
     * returns false for "0", "false", "off", "no", and ""
     * null otherwise. */
    match len {
        0 => {
            _ret = 0;
        }
        1 => {
            if str == "1" {
                _ret = -1;
            } else if str == "0" {
                _ret = -1;
            } else {
                _ret = -1;
            }
        }
        2 => {
            if str.to_lowercase() == "on" {
                _ret = 1;
            } else if str.to_lowercase() == "no" {
                _ret = 0;
            } else {
                _ret = -1;
            }
        }
        3 => {
            if str.to_lowercase() == "yes" {
                _ret = 1;
            } else if str.to_lowercase() == "off" {
                _ret = 0;
            } else {
                _ret = -1;
            }
        }
        4 => {
            if str.to_lowercase().as_str() == "true" {
                _ret = 1;
            } else {
                _ret = -1;
            }
        }
        5 => {
            if str.to_lowercase().as_str() == "false" {
                _ret = 0;
            } else {
                _ret = -1;
            }
        }
        _ => {
            _ret = -1;
        }
    }

    if _ret == -1 {
        Err(_ret)
    } else {
        Ok(cast_to_bool(_ret))
    }
}

fn cast_to_bool(num: i32) -> bool {
    num != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn cast_to_bool(i: i32) -> bool {
        i != 0
    }

    #[test]
    fn test_valid_true_values() {
        assert_eq!(parse_bool(&mut "true".to_string()), Ok(true));
        assert_eq!(parse_bool(&mut "on".to_string()), Ok(true));
        assert_eq!(parse_bool(&mut "yes".to_string()), Ok(true));
        // Add more cases for valid true values
    }

    #[test]
    fn test_valid_false_values() {
        assert_eq!(parse_bool(&mut "false".to_string()), Ok(false));
        assert_eq!(parse_bool(&mut "off".to_string()), Ok(false));
        assert_eq!(parse_bool(&mut "no".to_string()), Ok(false));
        assert_eq!(parse_bool(&mut "".to_string()), Ok(false));
        // Add more cases for valid false values
    }

    #[test]
    fn test_invalid_values() {
        assert_eq!(parse_bool(&mut "1".to_string()), Err(-1));
        assert_eq!(parse_bool(&mut "0".to_string()), Err(-1));
        assert_eq!(parse_bool(&mut "2".to_string()), Err(-1));
        assert_eq!(parse_bool(&mut "not a boolean".to_string()), Err(-1));
        // Add more cases for invalid values
    }

    #[test]
    fn test_case_insensitivity() {
        assert_eq!(parse_bool(&mut "TrUe".to_string()), Ok(true));
        assert_eq!(parse_bool(&mut "FaLsE".to_string()), Ok(false));
        // Add more cases for case insensitivity
    }
}
