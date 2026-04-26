public class Service {
    public Dog CreateDog(string name) {
        Dog dog = new Dog(name);
        if (!dog.Validate()) {
            throw new ArgumentException("invalid dog");
        }
        Connection conn = DatabaseHelper.GetConnection();
        conn.Execute("INSERT INTO dogs VALUES (?)");
        conn.Commit();
        return dog;
    }

    public Cat CreateCat(string name) {
        Cat cat = new Cat(name);
        if (!cat.Validate()) {
            throw new ArgumentException("invalid cat");
        }
        Connection conn = DatabaseHelper.GetConnection();
        conn.Execute("INSERT INTO cats VALUES (?)");
        conn.Commit();
        return cat;
    }

    public void TransferAnimal(object animal, Shelter shelter) {
        Transaction txn = new Transaction(DatabaseHelper.GetConnection());
        txn.Execute("UPDATE animals SET shelter_id = ?");
        shelter.Add(animal);
        txn.Commit();
    }

    public object ListAnimals() {
        Connection conn = DatabaseHelper.GetConnection();
        return conn.Execute("SELECT * FROM animals");
    }
}
