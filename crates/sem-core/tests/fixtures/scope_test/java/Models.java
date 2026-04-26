public class Dog {
    private String name;

    public Dog(String name) {
        this.name = name;
    }

    public String speak() {
        return "woof";
    }

    public boolean validate() {
        return name.length() > 0;
    }
}

class Cat {
    private String name;

    public Cat(String name) {
        this.name = name;
    }

    public String speak() {
        return "meow";
    }

    public boolean validate() {
        return name.length() > 0 && name.length() < 50;
    }
}

class Shelter {
    private List<Object> animals = new ArrayList<>();

    public void add(Object animal) {
        animals.add(animal);
    }

    public int count() {
        return animals.size();
    }
}
