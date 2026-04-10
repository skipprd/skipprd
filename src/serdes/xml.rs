use std::io::Read;

use quick_xml::events::Event;
use quick_xml::Reader;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum XmlDecodeError {
    #[error("XML read failed: {0}")]
    Read(#[from] std::io::Error),
    #[error("XML parse failed: {0}")]
    Parse(String),
}

pub struct SerdeXml;

#[derive(Debug)]
struct XmlNode {
    name: String,
    fields: serde_json::Map<String, serde_json::Value>,
    text: String,
}

impl XmlNode {
    fn new(name: String) -> Self {
        Self {
            name,
            fields: serde_json::Map::new(),
            text: String::new(),
        }
    }

    fn push_value(&mut self, key: String, value: serde_json::Value) {
        match self.fields.get_mut(&key) {
            Some(existing) => match existing {
                serde_json::Value::Array(values) => values.push(value),
                other => {
                    let previous = std::mem::replace(other, serde_json::Value::Null);
                    *other = serde_json::Value::Array(vec![previous, value]);
                }
            },
            None => {
                self.fields.insert(key, value);
            }
        }
    }

    fn into_value(mut self) -> serde_json::Value {
        let text = self.text.trim();
        if !text.is_empty() {
            if self.fields.is_empty() {
                return serde_json::Value::String(text.to_string());
            }
            self.fields.insert(
                "$text".to_string(),
                serde_json::Value::String(text.to_string()),
            );
        }

        if self.fields.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(self.fields)
        }
    }
}

impl SerdeXml {
    pub fn deserialize<R: Read>(mut reader: R) -> Result<Vec<serde_json::Value>, XmlDecodeError> {
        let mut xml = String::new();
        reader.read_to_string(&mut xml)?;
        let value = Self::parse_document(&xml)?;
        Ok(Self::normalize_root(value))
    }

    fn normalize_root(value: serde_json::Value) -> Vec<serde_json::Value> {
        match value {
            serde_json::Value::Array(values) => values,
            serde_json::Value::Object(root) => {
                let root_entries: Vec<(String, serde_json::Value)> = root.into_iter().collect();
                if root_entries.len() == 1 {
                    let (_, inner) = root_entries.into_iter().next().unwrap();
                    return match inner {
                        serde_json::Value::Array(values) => values,
                        serde_json::Value::Object(inner_map) => {
                            let inner_entries: Vec<(String, serde_json::Value)> =
                                inner_map.into_iter().collect();
                            if inner_entries.len() == 1 {
                                let (_, nested) = inner_entries.into_iter().next().unwrap();
                                return match nested {
                                    serde_json::Value::Array(values) => values,
                                    serde_json::Value::Object(nested_obj) => {
                                        vec![serde_json::Value::Object(nested_obj)]
                                    }
                                    other => vec![other],
                                };
                            }

                            vec![serde_json::Value::Object(
                                inner_entries.into_iter().collect(),
                            )]
                        }
                        other => vec![other],
                    };
                }

                vec![serde_json::Value::Object(
                    root_entries.into_iter().collect(),
                )]
            }
            other => vec![other],
        }
    }

    fn parse_document(xml: &str) -> Result<serde_json::Value, XmlDecodeError> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut buf = Vec::new();
        let mut stack: Vec<XmlNode> = Vec::new();
        let mut root: Option<serde_json::Value> = None;

        loop {
            match reader
                .read_event_into(&mut buf)
                .map_err(|err| XmlDecodeError::Parse(err.to_string()))?
            {
                Event::Start(start) => {
                    let mut node =
                        XmlNode::new(String::from_utf8_lossy(start.name().as_ref()).to_string());
                    for attribute in start.attributes() {
                        let attribute =
                            attribute.map_err(|err| XmlDecodeError::Parse(err.to_string()))?;
                        let key = format!("@{}", String::from_utf8_lossy(attribute.key.as_ref()));
                        let value = attribute
                            .decode_and_unescape_value(reader.decoder())
                            .map_err(|err| XmlDecodeError::Parse(err.to_string()))?
                            .into_owned();
                        node.push_value(key, serde_json::Value::String(value));
                    }
                    stack.push(node);
                }
                Event::Empty(start) => {
                    let mut node =
                        XmlNode::new(String::from_utf8_lossy(start.name().as_ref()).to_string());
                    for attribute in start.attributes() {
                        let attribute =
                            attribute.map_err(|err| XmlDecodeError::Parse(err.to_string()))?;
                        let key = format!("@{}", String::from_utf8_lossy(attribute.key.as_ref()));
                        let value = attribute
                            .decode_and_unescape_value(reader.decoder())
                            .map_err(|err| XmlDecodeError::Parse(err.to_string()))?
                            .into_owned();
                        node.push_value(key, serde_json::Value::String(value));
                    }
                    Self::attach_node(&mut stack, node, &mut root);
                }
                Event::Text(text) => {
                    if let Some(node) = stack.last_mut() {
                        node.text.push_str(
                            &text
                                .decode()
                                .map_err(|err| XmlDecodeError::Parse(err.to_string()))?,
                        );
                    }
                }
                Event::CData(text) => {
                    if let Some(node) = stack.last_mut() {
                        node.text.push_str(
                            &text
                                .decode()
                                .map_err(|err| XmlDecodeError::Parse(err.to_string()))?,
                        );
                    }
                }
                Event::End(_) => {
                    let node = stack.pop().ok_or_else(|| {
                        XmlDecodeError::Parse("Unexpected XML end tag".to_string())
                    })?;
                    Self::attach_node(&mut stack, node, &mut root);
                }
                Event::Eof => break,
                _ => {}
            }

            buf.clear();
        }

        root.ok_or_else(|| XmlDecodeError::Parse("XML document was empty".to_string()))
    }

    fn attach_node(stack: &mut Vec<XmlNode>, node: XmlNode, root: &mut Option<serde_json::Value>) {
        let node_name = node.name.clone();
        let node_value = node.into_value();
        if let Some(parent) = stack.last_mut() {
            parent.push_value(node_name, node_value);
        } else {
            *root = Some(serde_json::json!({ node_name: node_value }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SerdeXml;

    #[test]
    fn invalid_input_returns_error() {
        let err = SerdeXml::deserialize("<items><item></items>".as_bytes()).unwrap_err();
        assert!(err.to_string().contains("XML parse failed"));
    }

    #[test]
    fn repeated_elements_expand_into_records() {
        let records = SerdeXml::deserialize(
            r#"<items>
                <item><name>Cake</name><ppu>0.55</ppu></item>
                <item><name>Donut</name><ppu>1.25</ppu></item>
            </items>"#
                .as_bytes(),
        )
        .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["name"], "Cake");
        assert_eq!(records[1]["name"], "Donut");
    }
}
