func createDog(name: String) -> Dog {
    let dog = Dog(name: name)
    if !dog.validate() {
        fatalError("invalid dog")
    }
    let conn = getConnection()
    conn.execute(query: "INSERT INTO dogs VALUES (?)")
    conn.commit()
    return dog
}

func createCat(name: String) -> Cat {
    let cat = Cat(name: name)
    if !cat.validate() {
        fatalError("invalid cat")
    }
    let conn = getConnection()
    conn.execute(query: "INSERT INTO cats VALUES (?)")
    conn.commit()
    return cat
}

func transferAnimal(animal: Any, shelter: Shelter) {
    let txn = Transaction(conn: getConnection())
    txn.execute(query: "UPDATE animals SET shelter_id = ?")
    shelter.add(animal: animal)
    txn.commit()
}

func listAnimals() -> [Any] {
    let conn = getConnection()
    return conn.execute(query: "SELECT * FROM animals") as! [Any]
}
