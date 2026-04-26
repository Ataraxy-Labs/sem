require_relative 'service'
require_relative 'models'

def handle_create_dog(request)
  name = request["name"]
  create_dog(name)
end

def handle_create_cat(request)
  name = request["name"]
  create_cat(name)
end

def handle_transfer(request)
  shelter = Shelter.new
  dog = Dog.new(request["name"])
  transfer_animal(dog, shelter)
  shelter.count
end

def handle_list(request)
  list_animals
end

def validate(request)
  raise ArgumentError, "name required" unless request["name"]
  true
end
