use std::collections::HashMap;

pub struct Str {
    pub title: String,
    pub separator: String,
    pub language: String,
    pub studly_cache: HashMap<String, String>,
    pub camel_cache: HashMap<String, String>,
}

impl Str {

    pub fn slug(&self) -> String {
        let mut title = self.title.clone();
        let mut flip = String::from("-");
        let mut separator = String::from("-");
        let mut language = String::from("en");

        if self.separator != "" {
            separator = self.separator.clone();
        }

        if self.language != "" {
            language = self.language.clone();
        }

        if separator == "-" {
            flip = String::from("_");
        }

        title = title.replace(flip.as_str(), separator.as_str());
        title = title.replace("@", separator.as_str());
        title = title.replace(" ", separator.as_str());

        let mut title_chars: Vec<char> = title.chars().collect();
        let mut title_chars_len = title_chars.len();
        let mut title_chars_new: Vec<char> = Vec::new();

        for i in 0..title_chars_len {
            let mut char_to_add = title_chars[i];

            if char_to_add == '-' || char_to_add == '_' {
                char_to_add = ' ';
            }

            title_chars_new.push(char_to_add);
        }

        title = title_chars_new.into_iter().collect();

        let mut title_words: Vec<&str> = title.split(" ").collect();
        let mut title_words_len = title_words.len();
        let mut title_words_new: Vec<&str> = Vec::new();

        for i in 0..title_words_len {
            let mut word_to_add = title_words[i];

            if word_to_add != "" {
                title_words_new.push(word_to_add);
            }
        }

        title = title_words_new.join(separator.as_str());

        title
    }

    pub fn studly(value: &str) -> String {
        let key = value;

        if Str::studly_cache.contains_key(key) {
            return Str::studly_cache.get(key).unwrap().to_string();
        }

        let value = value.replace("-", " ").replace("_", " ");
        let value = value.split_whitespace().map(|s| s.to_uppercase()).collect::<Vec<String>>().join("");

        Str::studly_cache.insert(key.to_string(), value.to_string());

        return value;
    }

    pub fn camel(&self, value: &str) -> String {
        if self.camel_cache.contains_key(value) {
            return self.camel_cache.get(value).unwrap().to_string();
        }

        let camel_value = self.studly(value);
        let mut chars = camel_value.chars();
        let first_char = chars.next().unwrap();
        let camel_value = first_char.to_lowercase().collect::<String>() + chars.as_str();
        self.camel_cache.insert(value.to_string(), camel_value.clone());
        camel_value
    }

}