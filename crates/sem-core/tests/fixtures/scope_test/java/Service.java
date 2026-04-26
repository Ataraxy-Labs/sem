public class Service {
    public Dog createDog(String name) {
        Dog dog = new Dog(name);
        if (!dog.validate()) {
            throw new IllegalArgumentException("invalid dog");
        }
        Connection conn = DatabaseHelper.getConnection();
        conn.execute("INSERT INTO dogs VALUES (?)");
        conn.commit();
        return dog;
    }

    public Cat createCat(String name) {
        Cat cat = new Cat(name);
        if (!cat.validate()) {
            throw new IllegalArgumentException("invalid cat");
        }
        Connection conn = DatabaseHelper.getConnection();
        conn.execute("INSERT INTO cats VALUES (?)");
        conn.commit();
        return cat;
    }

    public void transferAnimal(Object animal, Shelter shelter) {
        Transaction txn = new Transaction(DatabaseHelper.getConnection());
        txn.execute("UPDATE animals SET shelter_id = ?");
        shelter.add(animal);
        txn.commit();
    }

    public Object listAnimals() {
        Connection conn = DatabaseHelper.getConnection();
        return conn.execute("SELECT * FROM animals");
    }
}
