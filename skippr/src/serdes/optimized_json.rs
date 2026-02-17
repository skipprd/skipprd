use serde_json::Value;

/// Optimized JSON parser that reduces allocations and improves performance
/// for special JSON formats (single quotes, unicode markers, concatenated JSON)
pub struct OptimizedJsonParser {
    enable_single_quotes: bool,
    enable_unicode: bool,
}

impl OptimizedJsonParser {
    pub fn new(enable_single_quotes: bool, enable_unicode: bool) -> Self {
        Self {
            enable_single_quotes,
            enable_unicode,
        }
    }

    /// Fast-path check to see if a string needs special processing
    pub fn needs_processing(&self, input: &str) -> bool {
        // If features are disabled, no processing needed
        if !self.enable_single_quotes && !self.enable_unicode {
            return false;
        }

        // Quick scan for special characters
        if self.enable_single_quotes && input.contains('\'') {
            return true;
        }

        if self.enable_unicode && input.contains("u'") {
            return true;
        }

        false
    }

    /// Parse JSON with optimized handling for special cases
    pub fn parse(&self, input: &str) -> Vec<Value> {
        // First try standard parsing
        match serde_json::from_str::<Value>(input) {
            Ok(Value::Array(values)) => return values,
            Ok(value) => return vec![value],
            Err(_) => {}
        }

        // If no special processing needed, use line-by-line parsing
        if !self.needs_processing(input) {
            let result = self.parse_line_by_line(input);
            if !result.is_empty() {
                return result;
            }
        }

        // Process input with optimized parser
        let result = self.parse_with_processing(input);

        // If we still have no valid JSON, try one more approach for strings before JSON content
        if result.is_empty() {
            // Find and extract any JSON-like content in the string
            if let Some(start) = input.find('{') {
                if let Some(end) = input[start..].rfind('}') {
                    let json_part = &input[start..=start + end];
                    if let Ok(value) = serde_json::from_str::<Value>(json_part) {
                        return vec![value];
                    }
                }
            }
        }

        result
    }

    /// Parse input line-by-line for better handling of multi-line JSON
    fn parse_line_by_line(&self, input: &str) -> Vec<Value> {
        let mut result = Vec::new();

        for line in input.lines() {
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<Value>(line) {
                Ok(value) => {
                    result.push(value);
                }
                Err(_) => {
                    // Try to handle concatenated JSON
                    if line.contains("}{") {
                        result.extend(self.parse_concatenated_json(line));
                    }
                }
            }
        }

        result
    }

    /// Parse input with special processing for single quotes and unicode markers
    fn parse_with_processing(&self, input: &str) -> Vec<Value> {
        let mut result = Vec::new();

        // Process each line
        for line in input.lines() {
            if line.trim().is_empty() {
                continue;
            }

            // Process the line
            let processed_line = self.process_line(line);

            // Try to parse the processed line
            match serde_json::from_str::<Value>(&processed_line) {
                Ok(value) => {
                    result.push(value);
                }
                Err(_) => {
                    // Try to handle concatenated JSON
                    if processed_line.contains("}{") {
                        result.extend(self.parse_concatenated_json(&processed_line));
                    }
                }
            }
        }

        result
    }

    /// Process a single line of JSON with optimized character-by-character parsing
    fn process_line(&self, line: &str) -> String {
        // Find the start of JSON content
        let start_idx = match line.find(|c| c == '{' || c == '[') {
            Some(idx) => idx,
            None => 0,
        };

        let line = &line[start_idx..];

        // Preallocate with extra capacity for quote replacements
        let mut result = String::with_capacity(line.len() + 20);

        // Single-pass processing of characters
        let mut in_double_quotes = false;
        let mut i = 0;
        let bytes = line.as_bytes();

        while i < bytes.len() {
            let c = bytes[i] as char;

            match c {
                '"' => {
                    in_double_quotes = !in_double_quotes;
                    result.push('"');
                }
                '\'' if self.enable_single_quotes && !in_double_quotes => {
                    // Replace single quotes with double quotes outside of double-quoted strings
                    result.push('"');
                }
                'u' if self.enable_unicode
                    && i + 1 < bytes.len()
                    && bytes[i + 1] as char == '\''
                    && !in_double_quotes =>
                {
                    // Handle u'...' pattern
                    result.push('"');
                    i += 1; // Skip the 'u' and next character (the single quote)

                    // Skip ahead to closing single quote
                    i += 1;
                    while i < bytes.len() && bytes[i] as char != '\'' {
                        result.push(bytes[i] as char);
                        i += 1;
                    }

                    if i < bytes.len() && bytes[i] as char == '\'' {
                        result.push('"');
                    }
                }
                _ if !c.is_ascii_control() => {
                    result.push(c);
                }
                // Skip control characters
                _ => {}
            }

            i += 1;
        }

        // Remove BOM if present
        if result.starts_with("efbbbf") {
            result = result.replace("efbbbf", "");
        }

        result
    }

    /// Parse concatenated JSON objects (like {"a":1}{"b":2})
    fn parse_concatenated_json(&self, input: &str) -> Vec<Value> {
        let mut result = Vec::new();
        let mut depth = 0;
        let mut start = 0;

        for (i, c) in input.char_indices() {
            match c {
                '{' => {
                    if depth == 0 {
                        start = i;
                    }
                    depth += 1;
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 && start <= i {
                        // Found a complete JSON object
                        let obj_str = &input[start..=i];

                        // Process special characters if needed
                        let obj_str = if self.needs_processing(obj_str) {
                            self.process_line(obj_str)
                        } else {
                            obj_str.to_string()
                        };

                        if let Ok(value) = serde_json::from_str::<Value>(&obj_str) {
                            result.push(value);
                        }
                    }
                }
                _ => {}
            }
        }

        // If we didn't find any objects with the depth tracking approach,
        // fall back to the simpler split method
        if result.is_empty() && input.contains("}{") {
            let parts: Vec<&str> = input.split("}{").collect();

            for (i, part) in parts.iter().enumerate() {
                let mut obj_str = part.to_string();

                // Add missing braces
                if i > 0 {
                    obj_str.insert(0, '{');
                }

                if i < parts.len() - 1 {
                    obj_str.push('}');
                }

                // Process special characters if needed
                let obj_str = if self.needs_processing(&obj_str) {
                    self.process_line(&obj_str)
                } else {
                    obj_str
                };

                if let Ok(value) = serde_json::from_str::<Value>(&obj_str) {
                    result.push(value);
                }
            }
        }

        result
    }
}
