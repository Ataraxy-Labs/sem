mod config;
mod parser;
mod types;

use config::{load_config, Config};
use parser::Parser;
use types::Entity;

fn main() {
    let config = load_config("config.toml");
    let parser = Parser::new(config);

    let content = std::fs::read_to_string("input.rs").unwrap();
    match parser.parse(&content) {
        Ok(entities) => {
            for entity in &entities {
                println!("{}", entity.display_name());
            }
            process_entities(entities);
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}

fn process_entities(entities: Vec<Entity>) {
    for entity in entities {
        println!("Processing: {}", entity);
    }
}
