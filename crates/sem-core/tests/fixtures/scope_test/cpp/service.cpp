#include "models.hpp"
#include "database.hpp"

Dog createDog(std::string name) {
    Dog dog(name);
    if (!dog.validate()) {
        throw std::invalid_argument("invalid dog");
    }
    Connection* conn = getConnection();
    conn->execute("INSERT INTO dogs VALUES (?)");
    conn->commit();
    return dog;
}

Cat createCat(std::string name) {
    Cat cat(name);
    if (!cat.validate()) {
        throw std::invalid_argument("invalid cat");
    }
    Connection* conn = getConnection();
    conn->execute("INSERT INTO cats VALUES (?)");
    conn->commit();
    return cat;
}

void transferAnimal(void* animal, Shelter* shelter) {
    Transaction txn(getConnection());
    txn.execute("UPDATE animals SET shelter_id = ?");
    shelter->add(animal);
    txn.commit();
}

void* listAnimals() {
    Connection* conn = getConnection();
    return conn->execute("SELECT * FROM animals");
}
