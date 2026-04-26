public class Dog {
    private string name;

    public Dog(string name) {
        this.name = name;
    }

    public string Speak() {
        return "woof";
    }

    public bool Validate() {
        return name.Length > 0;
    }
}

public class Cat {
    private string name;

    public Cat(string name) {
        this.name = name;
    }

    public string Speak() {
        return "meow";
    }

    public bool Validate() {
        return name.Length > 0 && name.Length < 50;
    }
}

public class Shelter {
    private List<object> animals = new List<object>();

    public void Add(object animal) {
        animals.Add(animal);
    }

    public int Count() {
        return animals.Count;
    }
}
