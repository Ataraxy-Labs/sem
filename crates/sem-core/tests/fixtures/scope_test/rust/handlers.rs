use crate::service::{create_dog, create_cat, transfer_animal, list_animals};
use crate::models::{Dog, Cat, Shelter};

pub fn handle_create_dog(name: String) -> Dog {
    create_dog(name)
}

pub fn handle_create_cat(name: String) -> Cat {
    create_cat(name)
}

pub fn handle_transfer(name: String) -> usize {
    let mut shelter = Shelter::new();
    let dog = Dog::new(name);
    transfer_animal(dog.name, &mut shelter);
    shelter.count()
}

pub fn handle_list() -> Vec<String> {
    let animals = list_animals();
    animals
}

pub fn validate(name: &str) -> bool {
    !name.is_empty()
}
