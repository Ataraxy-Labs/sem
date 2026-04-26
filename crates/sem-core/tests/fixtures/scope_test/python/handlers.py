from service import create_dog, create_cat, transfer_animal, list_animals
from models import Dog, Cat, Shelter


def handle_create_dog(request):
    name = request.get("name")
    return create_dog(name)


def handle_create_cat(request):
    name = request.get("name")
    return create_cat(name)


def handle_transfer(request):
    shelter = Shelter()
    dog = Dog(request.get("name"))
    transfer_animal(dog, shelter)
    return shelter.count()


def handle_list(request):
    animals = list_animals()
    return animals


def validate(request):
    if not request.get("name"):
        raise ValueError("name required")
    return True
