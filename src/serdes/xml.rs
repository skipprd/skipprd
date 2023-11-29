use serde::{Deserialize, Serialize};

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Lines, Read, Result, Write};
use std::path::Path;

use serde_value::Value;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct SerdeXml {
    pub SupportedCompressionTypes: Vec<String>,
    pub CompressionType: String,
    pub Fh: String,
    pub Records: Vec<String>,
}

impl SerdeXml {
    pub fn new() -> SerdeXml {
        SerdeXml {
            SupportedCompressionTypes: vec![
                String::from("VALUE_COMPRESSION"),
                String::from("NO_COMPRESSION"),
            ],
            CompressionType: String::from("NO_COMPRESSION"),
            Fh: String::from(""),
            Records: vec![],
        }
    }

    pub fn deserialize<R: Read>(reader: R) -> Vec<serde_json::Value> {
        let value: Vec<Value> = serde_xml_rs::from_reader(reader).unwrap();
        value.iter().map(|v| SerdeXml::convert_to_json(v)).collect()
    }

    fn convert_to_json(value: &serde_value::Value) -> serde_json::Value {
        match value {
            serde_value::Value::Unit => serde_json::Value::Null,
            serde_value::Value::Bool(b) => serde_json::Value::Bool(*b),
            serde_value::Value::I64(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
            serde_value::Value::F64(n) => serde_json::Value::Number(serde_json::Number::from_f64(*n).unwrap()),
            serde_value::Value::String(s) => serde_json::Value::String(s.clone()),
            serde_value::Value::Seq(seq) => {
                serde_json::Value::Array(seq.iter().map(|v| SerdeXml::convert_to_json(v)).collect())
            }
            serde_value::Value::Map(map) => {
                serde_json::Value::Object(
                    map.iter()
                        .map(|(k, v)| (SerdeXml::convert_to_json(k).to_string(), SerdeXml::convert_to_json(v)))
                        .collect(),
                )
            }
            serde_value::Value::Bytes(bytes) => serde_json::Value::String(String::from_utf8_lossy(bytes).to_string()),
            serde_value::Value::Char(c) => serde_json::Value::String(c.to_string()),
            serde_value::Value::Option(opt) => match opt {
                Some(v) => SerdeXml::convert_to_json(v),
                None => serde_json::Value::Null,
            },
            serde_value::Value::Newtype(v) => SerdeXml::convert_to_json(v),
            serde_value::Value::U8(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
            serde_value::Value::U16(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
            serde_value::Value::U32(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
            serde_value::Value::U64(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
            serde_value::Value::I8(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
            serde_value::Value::I16(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
            serde_value::Value::I32(n) => serde_json::Value::Number(serde_json::Number::from(*n)),
            serde_value::Value::F32(n) => serde_json::Value::Number(serde_json::Number::from_f64(*n as f64).unwrap()),

        }
    }

    // pub fn deserialize(record: &str) -> Vec<Value> {
    //     let mut deserializer = serde_xml_rs::de::Deserializer::new_from_reader(Cursor::new(record));
    //     let mut serializer = serde_value::Serializer::new();
    //     transcode(&mut deserializer, &mut serializer)?;
    //     let value = serializer.unpacked_value();
    //     // println!("{:?}", value);
    //     value
    //
    // }

    pub fn open_writer(&mut self, filename: String) {
        self.Fh = filename;
    }

    pub fn close_writer(&mut self) {
        let file = File::create(&self.Fh).unwrap();
        let mut writer = BufWriter::new(file);

        for data in &self.Records {
            if self.CompressionType == "VALUE_COMPRESSION" {
                // Implement VALUE_COMPRESSION logic here
            } else if self.CompressionType == "NO_COMPRESSION" {
                writeln!(writer, "{}", data).unwrap();
            }
        }
    }

    pub fn serialize(&mut self, record: Vec<String>) {
        let serialized = format!(
            "<SerdeXml>{}</SerdeXml>",
            record
                .iter()
                .map(|r| format!("<Records>{}</Records>", r))
                .collect::<Vec<String>>()
                .join("")
        );
        self.Records.push(serialized);
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

    // pub fn xml_decode(string: &str) -> Vec<Value> {
    //     let mut message: Vec<Value> = Vec::new();
    //
    //     match from_str::<Value>(string) {
    //         Ok(Value::Array(lines)) => {
    //             message.extend(lines);
    //         }
    //         Ok(line) => {
    //             message.push(line);
    //         }
    //         Err(_) => {
    //             let lines = string
    //                 .lines()
    //                 .map(|line| {
    //                     let mut cleaned_line = line
    //                         .replace('\\', "")
    //                         .replace("u'", "\"")
    //                         .replace('\'', "\"");
    //
    //                     let valid_chars: String = cleaned_line
    //                         .chars()
    //                         .filter(|c| !c.is_ascii_control())
    //                         .collect();
    //
    //                     if valid_chars.starts_with("efbbbf") {
    //                         cleaned_line = valid_chars.replace("efbbbf", "");
    //                     }
    //
    //                     if let Some(xml_start) = cleaned_line.find(|c| c == '[' || c == '{') {
    //                         cleaned_line.drain(..xml_start);
    //                     }
    //
    //                     cleaned_line
    //                 })
    //                 .collect::<Vec<_>>();
    //
    //             let mut deserialized_lines: Vec<Value> = lines
    //                 .iter()
    //                 .map(|line| from_str(line).unwrap_or_default())
    //                 .collect();
    //
    //             if deserialized_lines.is_empty()
    //                 || deserialized_lines.first().unwrap() == &Value::Null
    //             {
    //                 deserialized_lines.clear();
    //
    //                 for line in lines {
    //                     let records: Vec<&str> = line.split("}{").collect();
    //
    //                     for (i, record) in records.iter().enumerate() {
    //                         let mut record = record.to_string();
    //
    //                         if i != 0 {
    //                             record.insert(0, '{');
    //                         }
    //
    //                         if i != records.len() - 1 {
    //                             record.push('}');
    //                         }
    //
    //                         deserialized_lines
    //                             .push(from_str(&record).unwrap_or_default());
    //                     }
    //                 }
    //             }
    //
    //             message.extend(deserialized_lines);
    //         }
    //     }
    //
    //     message
    // }
}

#[test]
fn test_xml_serde() {
    // #[test]
    // fn test_basic_valid_xml() {
        let record: String = r#"<note><to>Tove</to><from>Jani</from><heading>Reminder</heading><body>Don't forget me this weekend!</body></note>"#.to_string();
        println!("{:?}", record);
        let _msg = SerdeXml::deserialize(record.as_bytes());
        // println!("{:?}", msg);
        // assert_eq!(msg.first().unwrap()., "Tove");
        // assert_eq!(msg["body".to_string()], &Value::String("jj".to_string()));
    // }
}

#[test]
fn test_xml_repeate_fields_serde() {
    // #[test]
    // fn test_basic_valid_xml() {
        let record: String = r#"<items>
   <item id="0001" type="donut">
      <name>Cake</name>
      <ppu>0.55</ppu>
      <batters>
         <batter id="1001">Regular</batter>
         <batter id="1002">Chocolate</batter>
         <batter id="1003">Blueberry</batter>
      </batters>
      <topping id="5001">None</topping>
      <topping id="5002">Glazed</topping>
      <topping id="5005">Sugar</topping>
      <topping id="5006">Sprinkles</topping>
      <topping id="5003">Chocolate</topping>
      <topping id="5004">Maple</topping>
   </item>
</items>"#.to_string();
        println!("{:?}", record);
        let _msg = SerdeXml::deserialize(record.as_bytes());
        // println!("{:?}", msg);
        // assert_eq!(msg.first().unwrap()., "Tove");
        // assert_eq!(msg["body".to_string()], &Value::String("jj".to_string()));
    // }
}