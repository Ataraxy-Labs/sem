#include <string>
#include <vector>

class Dog {
private:
    std::string name;
public:
    Dog(std::string name) : name(name) {}

    std::string speak() {
        return "woof";
    }

    bool validate() {
        return name.length() > 0;
    }
};

class Cat {
private:
    std::string name;
public:
    Cat(std::string name) : name(name) {}

    std::string speak() {
        return "meow";
    }

    bool validate() {
        return name.length() > 0 && name.length() < 50;
    }
};

class Shelter {
private:
    std::vector<void*> animals;
public:
    void add(void* animal) {
        animals.push_back(animal);
    }

    int count() {
        return animals.size();
    }
};
