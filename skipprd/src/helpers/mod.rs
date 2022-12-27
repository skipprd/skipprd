use std::any::TypeId;
use std::collections::HashMap;
use regex::Regex;
use std::env;
use memory_stats::memory_stats;
use std::fs::File;
use std::io::prelude::*;
use std::io::BufReader;
use std::path::Path;
use std::process::Command;
use std::str;
use rand::distributions::uniform::SampleRange;
use rand::Rng;
use serde_json::Value;

mod configuration;
use crate::helpers::configuration::Config;

pub struct Helpers {
    pub(crate) clean_field_cache: HashMap<String, bool>,
}

impl Helpers {
    // pub fn explode_field(field: &str) -> Vec<&str> {
    //     let mut field_haystack: Vec<&str> = Vec::new();
    //
    //     let string = field.replace(".", "_");
    //     let string = string.to_lowercase();
    //
    //     // let string = string.replace("_", " ");
    //     // let string = string.replace("-", " ");
    //     // let string = string.replace(" ", "_");
    //
    //     field_haystack = string.split("_").collect();
    //
    //     field_haystack
    // }

    // pub fn is_sequential_array_keys(arr: &[i32]) -> bool {
    pub fn is_sequential_array_keys(arr: &Vec<Value>) -> bool {
        let mut arr = arr.to_vec();
        // arr.sort();
        // if arr.first() != Some(&0) && arr.is_empty() {
        //     return false;
        // }

        // arr.iter().enumerate().all(|(i, &v)| v == i as i32)
        arr.iter().enumerate().all(|(i, v)| v.is_u64())
    }

    pub fn clean_field_name<'a>(&mut self, field: String) -> String {
        let mut clean = field.to_string();

        if !self.clean_field_cache.contains_key(&field) || self.clean_field_cache[&field] {
            if field.parse::<i32>().is_ok() {
                clean = "item_".to_string() + &field;
            }

            clean = clean.to_lowercase();

            let re = Regex::new(r"[^_0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ]").unwrap();
            clean = re.replace_all(&clean, "_").to_string();

            // let pattern = "/[^" + preg_quote(
            //     "_0123456789_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
            //     "/"
            // ) + "]/";
            // clean = preg_replace(pattern, "_", field);

            let x: &[_] = &['1', '2', '3', '4', '5', '6', '7', '8', '9'];
            clean = clean.trim_matches(x).to_string();
            // clean = ltrim(clean, "0123456789");

            // '_' at the beginning is common and probably allowable
            clean = clean.trim_start_matches("_").to_string();
            // clean = trim(clean, '_');

            if clean != field {
                self.clean_field_cache.insert(field.parse().unwrap(), true);
            }
        }

        clean.to_string()
    }

    // pub fn clean_array_field_names<T>(mut self, mut array: HashMap<String, T>) {
    //     for (field, value) in array.iter() {
    //         let field = self.clean_field_name(field);
    //         if TypeId::of::<T>() == TypeId::of::<HashMap<String, T>>() {
    //             self.clean_array_field_names(value);
    //         }
    //         array.insert(field, value);
    //     }
    // }

    pub fn random_password(length: usize) -> String {
        let mut rng = rand::thread_rng();
        let alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890";
        let mut pass = String::new();
        let alpha_length = alphabet.len() - 1;
        for _ in 0..length {
            let n = rng.gen_range(0..alpha_length);
            pass.push(alphabet.chars().nth(n).unwrap());
        }
        pass
    }

    pub fn random_str(length: usize) -> String {
        let mut rng = rand::thread_rng();
        let alphabet = "abcdefghijklmnopqrstuvwxyz";
        let mut pass = String::new();
        let alpha_length = alphabet.len() - 1;
        for _ in 0..length {
            let n = rng.gen_range(0..alpha_length);
            pass.push(alphabet.chars().nth(n).unwrap());
        }
        pass
    }

    pub fn flatten(array: &HashMap<String, String>, delimiter: &str, prefix: &str) -> HashMap<String, String> {
        let mut result: HashMap<String, String> = HashMap::new();
        for (key, value) in array {
            if value.contains("{") {
                let mut new_prefix = prefix.to_string();
                new_prefix.push_str(delimiter);
                new_prefix.push_str(key);
                result.extend(Self::flatten(array, delimiter, &new_prefix));
            } else {
                let mut new_prefix = prefix.to_string();
                new_prefix.push_str(delimiter);
                new_prefix.push_str(key);
                result.insert(new_prefix, value.to_string());
            }
        }
        result
    }

    pub fn mem_limit_reached() -> bool {
        let mem_limit = env::var("MEM_LIMIT").unwrap_or("0".to_string()).parse::<u32>().unwrap_or(0);
        let mut mem_usage = 0;
        if let Some(usage) = memory_stats() {
            mem_usage = usage.physical_mem as u32;
        }

        if mem_usage >= mem_limit {
            return true;
        }

        return false;
    }

}
