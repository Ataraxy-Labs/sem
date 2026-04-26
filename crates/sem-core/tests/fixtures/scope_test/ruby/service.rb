require_relative 'models'
require_relative 'database'

def create_dog(name)
  dog = Dog.new(name)
  raise ArgumentError, "invalid dog" unless dog.validate
  conn = get_connection
  conn.execute("INSERT INTO dogs VALUES (?)")
  conn.commit
  dog
end

def create_cat(name)
  cat = Cat.new(name)
  raise ArgumentError, "invalid cat" unless cat.validate
  conn = get_connection
  conn.execute("INSERT INTO cats VALUES (?)")
  conn.commit
  cat
end

def transfer_animal(animal, shelter)
  txn = Transaction.new(get_connection)
  txn.execute("UPDATE animals SET shelter_id = ?")
  shelter.add(animal)
  txn.commit
end

def list_animals
  conn = get_connection
  conn.execute("SELECT * FROM animals")
end
