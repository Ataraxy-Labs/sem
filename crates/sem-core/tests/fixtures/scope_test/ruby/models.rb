class Dog
  attr_reader :name

  def initialize(name)
    @name = name
  end

  def speak
    "woof"
  end

  def validate
    name.length > 0
  end
end

class Cat
  attr_reader :name

  def initialize(name)
    @name = name
  end

  def speak
    "meow"
  end

  def validate
    name.length > 0 && name.length < 50
  end
end

class Shelter
  def initialize
    @animals = []
  end

  def add(animal)
    @animals << animal
  end

  def count
    @animals.length
  end
end
