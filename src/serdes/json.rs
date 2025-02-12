use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Result};
use std::path::Path;

extern crate regex;
use regex::Regex;

#[derive(Serialize, Deserialize, Debug)]
pub struct SerdeJson {
    pub supported_compression_types: Vec<String>,
    pub compression_type: String,
    pub fh: String,
    pub records: Vec<String>,
}

impl SerdeJson {
    pub fn new() -> SerdeJson {
        SerdeJson {
            supported_compression_types: vec![
                String::from("VALUE_COMPRESSION"),
                String::from("NO_COMPRESSION"),
            ],
            compression_type: String::from("NO_COMPRESSION"),
            fh: String::from(""),
            records: vec![],
        }
    }

    pub fn deserialize(record: &str) -> Vec<Value> {
        let mut messages: Vec<Value> = Vec::new();

        let line: Vec<Value> = SerdeJson::json_decode(record);

        for item in line {
            if item.is_string() {
                match serde_json::from_str::<Value>(item.as_str().unwrap_or_default()) {
                    Ok(message) => messages.push(message),
                    Err(e) => println!("Couldn't deserialize message: {}", e),
                }
            } else {
                messages.push(item);
            }
        }

        messages
    }

    pub fn open_writer(&mut self, filename: String, _schema: Vec<Value>) {
        self.fh = filename;
    }

    pub fn close_writer(&mut self) {
        for _data in self.records.iter() {
            if self.compression_type == "VALUE_COMPRESSION" {
            } else if self.compression_type == "NO_COMPRESSION" {
            }
        }
    }

    pub fn serialize(&mut self, record: Vec<Value>, _schema: Vec<Value>) {
        self.records
            .push(serde_json::to_string(&record).unwrap_or_default());
    }

    // The output is wrapped in a Result to allow matching on errors
    // Returns an Iterator to the Reader of the lines of the file.
    pub fn read_lines<P>(filename: P) -> Result<Lines<BufReader<File>>>
        where
            P: AsRef<Path>,
    {
        let file = File::open(filename)?;
        Ok(BufReader::new(file).lines())
    }

    pub fn json_decode(string: &str) -> Vec<Value> {
        let mut message: Vec<Value> = Vec::new();

        match serde_json::from_str::<Value>(string) {
            Ok(Value::Array(lines)) => {
                message.extend(lines);
            }
            Ok(line) => {
                message.push(line);
            }
            Err(err) => {

                let mut error_lines: Vec<String> = Vec::new();

                string.lines().into_iter().for_each(|line| {
                    match serde_json::from_str::<Value>(line) {
                        Ok(decoded_line) => {
                            message.push(decoded_line);
                        }
                        Err(err) => {
                            error_lines.push(line.to_string());
                        }
                    };
                });

                if !error_lines.is_empty() {
                    let lines = error_lines
                        .into_iter()
                        .map(|line| {
                            let mut cleaned_line = line
                                // .replace('\\', "") // double escape
                            .replace("u'", "\'"); // unicode
                            // .replace('\'', "\""); // single quote # @todo - make this a config. we ended up dropping messages with single quotes in valid values
                            
                            let re = Regex::new(r#"u'([^']*)'"#).unwrap();
                            cleaned_line = re.replace_all(&cleaned_line, "\"$1\"").to_string();


                            // @todo -support values containing single quotes e.g. "b'H'", also fix single quoted field and values {'status': '200'} -> {"status": "200"}

                            let valid_chars: String = cleaned_line
                                .chars()
                                .filter(|c| !c.is_ascii_control())
                                .collect();

                            if valid_chars.starts_with("efbbbf") {
                                cleaned_line = valid_chars.replace("efbbbf", "");
                            }

                            if let Some(json_start) = cleaned_line.find(|c| c == '[' || c == '{') {
                                cleaned_line.drain(..json_start);
                            }

                            cleaned_line
                        })
                        .collect::<Vec<_>>();


                    let mut deserialized_lines: Vec<Value> = lines
                        .iter()
                        .map(|line| serde_json::from_str(line).unwrap_or_default())
                        .collect();

                    if deserialized_lines.is_empty()
                        || deserialized_lines.first().unwrap() == &Value::Null
                    {
                        deserialized_lines.clear();

                        for line in lines {
                            let records: Vec<&str> = line.split("}{").collect();

                            for (i, record) in records.iter().enumerate() {
                                let mut record = record.to_string();

                                if i != 0 {
                                    record.insert(0, '{');
                                }

                                if i != records.len() - 1 {
                                    record.push('}');
                                }

                                deserialized_lines
                                    .push(serde_json::from_str(&record).unwrap_or_default());
                            }
                        }
                    }

                    message.extend(deserialized_lines);
                }
            }
        }

        message
    }
}



#[cfg(test)]
mod json_serde_tests {
    use super::*;

    #[test]
    fn test_basic_valid_json_test() {
        let record: String = r#"{"status": "200"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
    }

    #[test]
    fn test_nested_valid_json_2() {
        let record: String = r#"{"status": "200", "items": {"foo": "bar"}}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        assert_eq!(msg.first().unwrap()["items"]["foo"], "bar");
    }

    #[test]
    fn test_nested_array_valid_json() {
        let record: String = r#"{"status": "200", "items": [{"foo": "bar"}]}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        assert_eq!(msg.first().unwrap()["items"][0]["foo"], "bar");
    }

    // #[test]
    // fn test_escaped_json() {
    //     let record: String = r#"{\"status\": \"200\"}"#.to_string();
    //     let msg = SerdeJson::deserialize(&record);
    //     assert_eq!(msg.first().unwrap()["status"], "200");
    // }

    // #[test]
    // fn test_double_escaped_json() {
    //     let record: String = r#"{\\\"time\\\":{\\\"start_time\\\":\\\"273.046328210292\\\",\\\"end_time\\\":\\\"16182\\\"},\\\"bike_id\\\":\\\"0.579087190592872\\\",\\\"location\\\":{\\\"start\\\":\\\"0.620131100002421\\\",\\\"end\\\":null}}"#.to_string();
    //     let msg = SerdeJson::deserialize(&record);
    //     assert_eq!(msg.first().unwrap()["bike_id"], "0.579087190592872");
    //     assert_eq!(
    //         msg.first().unwrap()["time"]["start_time"],
    //         "273.046328210292"
    //     );
    // }

    #[test]
    fn test_null_value_valid_json() {
        let record: String = r#"{"start":"0.620131100002421","end":null}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["start"], "0.620131100002421");
        assert_eq!(msg.first().unwrap()["end"], Value::Null);
    }

    #[test]
    fn test_single_quote_value_json() {
        let record: String = r#"{"binary": "b'H'"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["binary"], "b'H'");
    }

    #[test]
    fn test_single_quote_strings_json() {
        let record: String = r#"{'status': '200'}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
    }

    #[test]
    fn test_string_before_json() {
        let record: String = r#"some, string, that exists)/ 20080808115538 {"status":"200","length":"4742","mime":"text/html","offset":"16518203"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        // assert!(msg.first().unwrap().get("some, string").is_none());
    }

    // #[test]
    // fn test_string_before_escaped_json() {
    //     let record: String = r#"some, string, that exists)/ 20080808115538 {\"status\":\"200\",\"length\":\"4742\",\"mime\":\"text/html\",\"offset\":\"16518203\"}"#.to_string();
    //     let msg = SerdeJson::deserialize(&record);
    //     assert_eq!(msg.first().unwrap()["status"], "200");
    //     // assert!(msg.first().unwrap().get("some, string").is_none());
    // }

    #[test]
    fn test_unicode_string_json() {
        let record: String = r#"{u'status': u'200'}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
    }

    #[test]
    fn test_unicode_string_value_json() {
        let record: String = r#"{"status": "\u0023"}"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "#");
        assert_ne!(msg.first().unwrap()["status"], 2605);
    }

    #[test]
    fn test_multi_record_array_json() {
        let record: String = r#"[{"status": "200"},{"status": "500"}]"#.to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        assert_eq!(msg.last().unwrap()["status"], "500");
    }

    #[test]
    fn test_valid_multi_line_json() {
        let record: String = "{\"status\": \"200\"}\n{\"status\": \"500\"}".to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()["status"], "200");
        assert_eq!(msg.last().unwrap()["status"], "500");
    }

    #[test]
    fn test_valid_multi_line_with_multi_record_arrays_json() {
        let record: String = "[{\"status\": \"200\"},{\"status\": \"201\"}]\n[{\"status\": \"202\"},{\"status\": \"203\"}]".to_string();
        let msg = SerdeJson::deserialize(&record);
        assert_eq!(msg.first().unwrap()[0]["status"], "200");
        assert_eq!(msg.first().unwrap()[1]["status"], "201");
        assert_eq!(msg.last().unwrap()[0]["status"], "202");
        assert_eq!(msg.last().unwrap()[1]["status"], "203");
    }

    /**
     * This madness is common, for instance AWS Firehose S3 Destination produces this crap
     */
    #[test]
    fn test_single_line_objects_json() {
        let record: String =
            r#"{"foo": {"nest": "bar"}}{"foo": {"nest": "baz"}}{"foo": {"nest": "boo"}}"#
                .to_string();
        let msg = SerdeJson::deserialize(&record);
        // assert!( msg.first().unwrap().is_array());
        assert_eq!(msg[0]["foo"]["nest"], "bar");
        assert_eq!(msg[1]["foo"]["nest"], "baz");
        assert_eq!(msg[2]["foo"]["nest"], "boo");
    }


    // #[test]
    // fn test_single_line_objects_json_cc() {
    //     let record: String =
    //         r#"{"IMEI": 359206105980999, "drum": {"data_valid": true, "speed_mean_rpm": 0.0, "speed_values_used": 7, "revolutions": 0.0, "low_latency_rpm": 0.0, "is_charging": false, "angle_degrees": 0.0, "vector_rpm": 0.0}, "pressure_a_bar": {"data_valid": true, "mean": 0.0, "median": 0.0, "sd": 0.0, "minimum": 0.0, "maximum": 0.0, "values_used": 2001, "temperature_degc": 12.6, "low_latency": 0.0}, "pressure_b_bar": {"data_valid": true, "mean": 0.02, "median": 0.0, "sd": 0.03, "minimum": 0.0, "maximum": 0.08, "values_used": 2001, "temperature_degc": 10.6, "low_latency": 0.02}, "supply_voltage": {"data_valid": true, "mean": 25.974, "sd": 0.0, "minimum": 25.974, "maximum": 25.974}, "gps": {"data_valid": true, "satellites_used": 20, "ehpes_m": [2.0, 2.1, 2.0, 2.0, 2.0, 2.1, 2.1, 2.0, 2.0, 2.1], "datetime_posix_utc_seconds": 1695256723, "latitude_decimal": 51.520846666666664, "longitude_decimal": 0.13453500000000002, "latitudes_decimal": [51.520846666666664, 51.520846666666664, 51.520846666666664, 51.520846666666664, 51.520846666666664, 51.520846666666664, 51.520846666666664, 51.520846666666664, 51.520846666666664, 51.520846666666664], "longitudes_decimal": [0.1345384, 0.1345384, 0.1345384, 0.1345384, 0.1345384, 0.1345367, 0.1345367, 0.1345367, 0.13453500000000002, 0.13453500000000002], "datetimes_posix_utc_seconds": [1695256714, 1695256715, 1695256716, 1695256717, 1695256718, 1695256719, 1695256720, 1695256721, 1695256722, 1695256723], "ephe_m": 2.1}, "modem": {"rssi_dbm": "-59 dBm", "bit_error_rate_pc": "3.2%:6.4%", "access_technology": "Cat M1"}, "system": {"cpu_temperature_degc": 43.0, "operating_mode": "Unknown key: b'H'", "datetime_posix_utc_seconds": 1695256724, "enclosure_temperature_degc": 17.0, "enclosure_humidity_rh": 78.0, "error_flags": "", "12v_bus_current_a": 0.22}, "imu": {"data_valid": true, "temperature_degC": 26.0, "xy_angle": [0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4], "zx_angle": [0.65, 0.65, 0.65, 0.65, 0.65, 0.65, 0.65, 0.65, 0.65, 0.65], "x_axis_linear_max": [0.055, 0.053, 0.048, 0.072, 0.05, 0.055, 0.057, 0.06, 0.055, 0.048], "y_axis_linear_max": [0.061, 0.059, 0.054, 0.063, 0.044, 0.059, 0.052, 0.059, 0.073, 0.044], "z_axis_linear_max": [0.043, 0.05, 0.045, 0.057, 0.057, 0.048, 0.043, 0.055, 0.043, 0.04], "x_axis_linear_mean": [0.001, 0.0, 0.002, 0.001, 0.002, 0.001, 0.001, 0.001, 0.0, 0.002], "y_axis_linear_mean": [0.001, 0.002, 0.001, 0.0, -0.001, 0.001, 0.001, 0.002, 0.001, 0.0], "z_axis_linear_mean": [0.001, 0.002, 0.0, 0.001, 0.002, 0.001, 0.001, 0.001, 0.002, 0.001], "values_used": 10}, "temperature_module": {"surface_temperature_degc": 0.0, "second_input_temperature_degc": 0.0, "speed_mean_rpm": 0.0, "angle_degrees": 0.0, "status": "Not Present", "data_valid": false, "revolutions": 0.0}, "reference_weight_kimax2": {"ch1": {"weight_kg": 0.0, "load_kg": 0.0, "tare_kg": 0.0}, "ch2": {"weight_kg": 0.0, "load_kg": 0.0, "tare_kg": 0.0}, "ch3": {"weight_kg": 0.0, "load_kg": 0.0, "tare_kg": 0.0}, "data_valid": false}, "water_flowmeter": {"total_volume_m3": 0.0, "flow_rate_m3/hr": 0.0, "temperature_degc": 0.0, "data_valid": false}, "backend_metadata": {"received_time": "2023-09-21T00:38:44.198177Z"}, "truck": {"gearbox_ratio": 120.3, "motor_efficiency": 0.9, "motor_displacement_cm3": 89.1, "rmc_provider": "Cemex", "registration": "KS17TKK", "id": 161}}{"IMEI": 359206105981088, "drum": {"data_valid": true, "speed_mean_rpm": 0.0, "speed_values_used": 7, "revolutions": 0.0, "low_latency_rpm": 0.0, "is_charging": true, "angle_degrees": 0.0, "vector_rpm": 0.0}, "pressure_a_bar": {"data_valid": true, "mean": 0.0, "median": 0.0, "sd": 0.0, "minimum": 0.0, "maximum": 0.0, "values_used": 2009, "temperature_degc": 10.5, "low_latency": 0.0}, "pressure_b_bar": {"data_valid": true, "mean": 0.0, "median": 0.0, "sd": 0.0, "minimum": 0.0, "maximum": 0.0, "values_used": 2009, "temperature_degc": 10.0, "low_latency": 0.0}, "supply_voltage": {"data_valid": true, "mean": 25.885, "sd": 0.0, "minimum": 25.885, "maximum": 25.885}, "gps": {"data_valid": true, "satellites_used": 17, "ehpes_m": [2.2, 2.2, 2.2, 2.2, 2.2, 2.2, 2.1, 2.1, 2.2, 2.2], "datetime_posix_utc_seconds": 1695256724, "latitude_decimal": 51.68170833333333, "longitude_decimal": -0.01842, "latitudes_decimal": [51.68171003333333, 51.68171003333333, 51.68171003333333, 51.68171003333333, 51.68171003333333, 51.68171003333333, 51.68170833333333, 51.68170833333333, 51.68170833333333, 51.68170833333333], "longitudes_decimal": [-0.0184217, -0.0184217, -0.0184217, -0.0184217, -0.0184217, -0.0184217, -0.0184217, -0.01842, -0.01842, -0.01842], "datetimes_posix_utc_seconds": [1695256715, 1695256716, 1695256717, 1695256718, 1695256719, 1695256720, 1695256721, 1695256722, 1695256723, 1695256724], "ephe_m": 2.2}, "modem": {"rssi_dbm": "-61 dBm", "bit_error_rate_pc": "0.4%:0.8%", "access_technology": "Cat M1"}, "system": {"cpu_temperature_degc": 42.0, "operating_mode": "Unknown key: b'H'", "datetime_posix_utc_seconds": 1695256725, "enclosure_temperature_degc": 17.0, "enclosure_humidity_rh": 80.0, "error_flags": "", "12v_bus_current_a": 0.21}, "imu": {"data_valid": true, "temperature_degC": 25.0, "xy_angle": [1.14, 1.14, 1.14, 1.14, 1.14, 1.14, 1.14, 1.14, 1.14, 1.14], "zx_angle": [-0.09, -0.09, -0.1, -0.1, -0.1, -0.1, -0.1, -0.1, -0.1, -0.1], "x_axis_linear_max": [0.066, 0.054, 0.051, 0.056, 0.047, 0.059, 0.056, 0.051, 0.044, 0.049], "y_axis_linear_max": [0.053, 0.053, 0.046, 0.055, 0.053, 0.048, 0.053, 0.055, 0.053, 0.046], "z_axis_linear_max": [0.044, 0.047, 0.042, 0.045, 0.042, 0.042, 0.04, 0.045, 0.043, 0.05], "x_axis_linear_mean": [0.003, 0.002, 0.001, 0.001, 0.002, 0.002, 0.001, 0.002, -0.001, 0.0], "y_axis_linear_mean": [0.001, 0.003, 0.0, 0.0, 0.002, 0.001, 0.002, 0.001, 0.001, 0.0], "z_axis_linear_mean": [0.0, 0.0, 0.0, 0.0, 0.001, -0.001, 0.0, 0.001, 0.002, 0.002], "values_used": 10}, "temperature_module": {"surface_temperature_degc": 0.0, "second_input_temperature_degc": 0.0, "speed_mean_rpm": 0.0, "angle_degrees": 0.0, "status": "Not Present", "data_valid": false, "revolutions": 0.0}, "reference_weight_kimax2": {"ch1": {"weight_kg": 0.0, "load_kg": 0.0, "tare_kg": 0.0}, "ch2": {"weight_kg": 0.0, "load_kg": 0.0, "tare_kg": 0.0}, "ch3": {"weight_kg": 0.0, "load_kg": 0.0, "tare_kg": 0.0}, "data_valid": false}, "water_flowmeter": {"total_volume_m3": 0.0, "flow_rate_m3/hr": 0.0, "temperature_degc": 0.0, "data_valid": false}, "backend_metadata": {"received_time": "2023-09-21T00:38:45.598036Z"}, "truck": {"gearbox_ratio": 120.3, "motor_efficiency": 0.9, "motor_displacement_cm3": 89.1, "rmc_provider": "Cemex", "registration": "RX16WYC", "id": 156}}"#
    //             .to_string();
    //     let msg = SerdeJson::deserialize(&record);
    //     // assert!( msg.first().unwrap().is_array());
    //     assert_eq!(msg[0]["IMEI"], 359206105980999 as i64);
    //     assert_eq!(msg[1]["supply_voltage"]["mean"], 25.885); // supply_voltage": {"data_valid": true, "mean": 25.885
    //     // assert_eq!(msg[2]["gps"]["datetime_posix_utc_seconds"], "1695256726"); // gps": {"data_valid": true, "satellites_used": 18, "ehpes_m": [3.0, 3.0, 3.0, 3.0, 2.9, 2.9, 2.9, 2.9, 2.8, 2.8], "datetime_posix_utc_seconds": 1695256726
    // }

}
