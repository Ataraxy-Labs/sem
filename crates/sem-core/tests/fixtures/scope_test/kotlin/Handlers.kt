fun handleCreateDog(request: Map<String, Any>): Any {
    val name = request["name"] as String
    return createDog(name)
}

fun handleCreateCat(request: Map<String, Any>): Any {
    val name = request["name"] as String
    return createCat(name)
}

fun handleTransfer(request: Map<String, Any>): Int {
    val shelter = Shelter()
    val dog = Dog(request["name"] as String)
    transferAnimal(dog, shelter)
    return shelter.count()
}

fun handleList(request: Map<String, Any>): List<Any> {
    val animals = listAnimals()
    return animals
}

fun validate(request: Map<String, Any>): Boolean {
    if (request["name"] == null) {
        throw IllegalArgumentException("name required")
    }
    return true
}

fun handleRegistry(registry: Registry) {
    val shelter = registry.shelter
    shelter.add("dog")
}
