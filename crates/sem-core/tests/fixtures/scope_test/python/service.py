from models import Dog, Cat, Shelter
from database import get_connection, Transaction


def create_dog(name):
    dog = Dog(name)
    if not dog.validate():
        raise ValueError("invalid dog")
    conn = get_connection()
    conn.execute("INSERT INTO dogs VALUES (?)")
    conn.commit()
    return dog


def create_cat(name):
    cat = Cat(name)
    if not cat.validate():
        raise ValueError("invalid cat")
    conn = get_connection()
    conn.execute("INSERT INTO cats VALUES (?)")
    conn.commit()
    return cat


def transfer_animal(animal, shelter):
    txn = Transaction(get_connection())
    txn.execute("UPDATE animals SET shelter_id = ?")
    shelter.add(animal)
    txn.commit()


def list_animals():
    conn = get_connection()
    return conn.execute("SELECT * FROM animals")
