import { Dog, Cat, Shelter } from './models';
import { getConnection, Transaction } from './database';

export function createDog(name: string): Dog {
    const dog = new Dog(name);
    if (!dog.validate()) {
        throw new Error("invalid dog");
    }
    const conn = getConnection();
    conn.execute("INSERT INTO dogs VALUES (?)");
    conn.commit();
    return dog;
}

export function createCat(name: string): Cat {
    const cat = new Cat(name);
    if (!cat.validate()) {
        throw new Error("invalid cat");
    }
    const conn = getConnection();
    conn.execute("INSERT INTO cats VALUES (?)");
    conn.commit();
    return cat;
}

export function transferAnimal(animal: any, shelter: Shelter): void {
    const txn = new Transaction(getConnection());
    txn.execute("UPDATE animals SET shelter_id = ?");
    shelter.add(animal);
    txn.commit();
}

export function listAnimals(): any[] {
    const conn = getConnection();
    return conn.execute("SELECT * FROM animals");
}
