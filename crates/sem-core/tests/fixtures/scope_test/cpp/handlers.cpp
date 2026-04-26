#include "models.hpp"
#include "service.cpp"

Dog handleCreateDog(Request* request) {
    std::string name = request->get("name");
    return createDog(name);
}

Cat handleCreateCat(Request* request) {
    std::string name = request->get("name");
    return createCat(name);
}

int handleTransfer(Request* request) {
    Shelter shelter;
    Dog dog(request->get("name"));
    transferAnimal(&dog, &shelter);
    return shelter.count();
}

void* handleList(Request* request) {
    return listAnimals();
}

bool validate(Request* request) {
    if (request->get("name").empty()) {
        throw std::invalid_argument("name required");
    }
    return true;
}
