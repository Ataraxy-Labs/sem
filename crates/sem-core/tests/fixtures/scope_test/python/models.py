class Dog:
    def __init__(self, name):
        self.name = name

    def speak(self):
        return "woof"

    def validate(self):
        return len(self.name) > 0


class Cat:
    def __init__(self, name):
        self.name = name

    def speak(self):
        return "meow"

    def validate(self):
        return len(self.name) > 0 and len(self.name) < 50


class Shelter:
    def __init__(self):
        self.animals = []

    def add(self, animal):
        if animal.validate():
            self.animals.append(animal)

    def count(self):
        return len(self.animals)
