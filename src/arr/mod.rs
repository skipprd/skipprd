use std::collections::HashMap;

pub struct Arr {}

impl Arr {
    pub fn get<'a, T>(
        array: &'a HashMap<String, T>,
        _key: &str,
        _default: &str,
    ) -> (&'static str, &'a HashMap<String, T>) {
        // @todo - field value may be a string or a nested complex type
        ("", array)

        // if key.is_empty() {
        //     return ("", array);
        // }
        //
        // if array.contains_key(key) {
        //     // return array[key].to_string();
        //     return (array[key], array);
        // }
        //
        // if !key.contains(".") {
        //     if array[key].is_empty() {
        //         return (default, array)
        //     } else {
        //         return array[key];
        //     }
        // }
        //
        // let mut array_clone = array;
        //
        // for segment in key.split(".") {
        //     if array.contains_key(segment) {
        //         array_clone = array[segment];
        //     } else {
        //         return (default, array);
        //     }
        // }
        //
        // return ("", array_clone);
    }
}
