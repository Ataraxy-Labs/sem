func handleCreateDog(request: [String: Any]) -> Any {
    let name = request["name"] as! String
    return createDog(name: name)
}

func handleCreateCat(request: [String: Any]) -> Any {
    let name = request["name"] as! String
    return createCat(name: name)
}

func handleTransfer(request: [String: Any]) -> Int {
    let shelter = Shelter()
    let dog = Dog(name: request["name"] as! String)
    transferAnimal(animal: dog, shelter: shelter)
    return shelter.count()
}

func handleList(request: [String: Any]) -> [Any] {
    let animals = listAnimals()
    return animals
}

func validate(request: [String: Any]) -> Bool {
    guard let _ = request["name"] else {
        fatalError("name required")
    }
    return true
}
