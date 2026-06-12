class Dog(val name: String) {
    fun speak(): String {
        return "woof"
    }

    fun validate(): Boolean {
        return name.length > 0
    }
}

class Cat(val name: String) {
    fun speak(): String {
        return "meow"
    }

    fun validate(): Boolean {
        return name.length > 0 && name.length < 50
    }
}

class Shelter {
    private val animals = mutableListOf<Any>()

    fun add(animal: Any) {
        animals.add(animal)
    }

    fun count(): Int {
        return animals.size
    }
}

class Registry(val shelter: Shelter)
