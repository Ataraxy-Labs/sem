use crate::models::{Dog, Cat, Shelter};
use crate::database::{get_connection, Transaction};

pub fn create_dog(name: String) -> Dog {
    let dog = Dog::new(name);
    if !dog.validate() {
        panic!("invalid dog");
    }
    let conn = get_connection();
    conn.execute("INSERT INTO dogs VALUES (?)");
    conn.commit();
    dog
}

pub fn create_cat(name: String) -> Cat {
    let cat = Cat::new(name);
    if !cat.validate() {
        panic!("invalid cat");
    }
    let conn = get_connection();
    conn.execute("INSERT INTO cats VALUES (?)");
    conn.commit();
    cat
}

pub fn transfer_animal(name: String, shelter: &mut Shelter) {
    let txn = Transaction::new(get_connection());
    txn.execute("UPDATE animals SET shelter_id = ?");
    shelter.add(name);
    txn.commit();
}

pub fn list_animals() -> Vec<String> {
    let conn = get_connection();
    conn.execute("SELECT * FROM animals")
}
