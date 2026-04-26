use std::collections::HashMap;

pub struct Config {
    pub values: HashMap<String, String>,
    pub debug: bool,
}

impl Config {
    pub fn new() -> Self {
        Config {
            values: HashMap::new(),
            debug: false,
        }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.values.get(key)
    }

    pub fn set(&mut self, key: String, value: String) {
        self.values.insert(key, value);
    }
}

pub fn load_config(path: &str) -> Config {
    let mut config = Config::new();
    config.set("path".to_string(), path.to_string());
    config
}
