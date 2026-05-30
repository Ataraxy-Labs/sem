fun createDog(name: String): Dog {
    val dog = Dog(name)
    if (!dog.validate()) {
        throw IllegalArgumentException("invalid dog")
    }
    val conn = getConnection()
    conn.execute("INSERT INTO dogs VALUES (?)")
    conn.commit()
    return dog
}

fun createCat(name: String): Cat {
    val cat = Cat(name)
    if (!cat.validate()) {
        throw IllegalArgumentException("invalid cat")
    }
    val conn = getConnection()
    conn.execute("INSERT INTO cats VALUES (?)")
    conn.commit()
    return cat
}

fun transferAnimal(animal: Any, shelter: Shelter) {
    val txn = Transaction(getConnection())
    txn.execute("UPDATE animals SET shelter_id = ?")
    shelter.add(animal)
    txn.commit()
}

fun listAnimals(): List<Any> {
    val conn = getConnection()
    return conn.execute("SELECT * FROM animals") as List<Any>
}
