class Dog {
    var name: String

    init(name: String) {
        self.name = name
    }

    func speak() -> String {
        return "woof"
    }

    func validate() -> Bool {
        return name.count > 0
    }
}

class Cat {
    var name: String

    init(name: String) {
        self.name = name
    }

    func speak() -> String {
        return "meow"
    }

    func validate() -> Bool {
        return name.count > 0 && name.count < 50
    }
}

class Shelter {
    var animals: [Any] = []

    func add(animal: Any) {
        animals.append(animal)
    }

    func count() -> Int {
        return animals.count
    }
}
