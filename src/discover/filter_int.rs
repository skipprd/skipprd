const FILTER_FLAG_ALLOW_OCTAL: bool = true;
const FILTER_FLAG_ALLOW_HEX: bool = true;

fn php_filter_int(value: &mut zval, flags: zend_long, option_array: &mut HashTable) -> Result<(), ()> {
    let mut min_range: zend_long = 0;
    let mut max_range: zend_long = 0;
    let mut option_flags: zend_long = 0;
    let mut min_range_set: bool = false;
    let mut max_range_set: bool = false;
    let mut allow_octal: bool = false;
    let mut allow_hex: bool = false;
    let mut len: size_t = 0;
    let mut error: bool = false;
    let mut ctx_value: zend_long = 0;
    let mut p: *const c_char = null_mut();

    // Parse options
    if let Some(option_val) = zend_hash_str_find(option_array, "min_range", strlen("min_range")) {
        min_range = zval_get_long(option_val);
        min_range_set = true;
    }
    if let Some(option_val) = zend_hash_str_find(option_array, "max_range", strlen("max_range")) {
        max_range = zval_get_long(option_val);
        max_range_set = true;
    }
    option_flags = flags;

    len = value.len();

    if len == 0 {
        return Err(());
    }

    if option_flags & FILTER_FLAG_ALLOW_OCTAL != 0 {
        allow_octal = true;
    }

    if option_flags & FILTER_FLAG_ALLOW_HEX != 0 {
        allow_hex = true;
    }

    // Start the validating loop
    p = Z_STRVAL_P(value);
    ctx_value = 0;

    PHP_FILTER_TRIM_DEFAULT(p, len);

    if *p == '0' {
        p = p.offset(1);
        len -= 1;
        if allow_hex && (*p == 'x' || *p == 'X') {
            p = p.offset(1);
            len -= 1;
            if len == 0 {
                return Err(zend_error_handling::FAILURE);
            }
            if php_filter_parse_hex(p, len, &ctx_value) < 0 {
                error = true;
            }
        } else if allow_octal {
            // Support explicit octal prefix notation
            if *p == 'o' || *p == 'O' {
                p = p.offset(1);
                len -= 1;
                if len == 0 {
                    return Err(zend_error_handling::FAILURE);
                }
            }
            if php_filter_parse_octal(p, len, &ctx_value) < 0 {
                error = true;
            }
        } else if len != 0 {
            error = true;
        }
    } else {
        if php_filter_parse_int(p, len, &ctx_value) < 0 {
            error = true;
        }
    }

    if error || (min_range_set && (ctx_value < min_range)) || (max_range_set && (ctx_value > max_range)) {
        return Err(zend_error_handling::FAILURE);
    } else {
        zval_ptr_dtor(value);
        ZVAL_LONG(value, ctx_value);
        return Ok(());
    }
}
