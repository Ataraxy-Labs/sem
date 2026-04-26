public class Handlers {
    private Service service = new Service();

    public Dog HandleCreateDog(Request request) {
        string name = request.Get("name");
        return service.CreateDog(name);
    }

    public Cat HandleCreateCat(Request request) {
        string name = request.Get("name");
        return service.CreateCat(name);
    }

    public int HandleTransfer(Request request) {
        Shelter shelter = new Shelter();
        Dog dog = new Dog(request.Get("name"));
        service.TransferAnimal(dog, shelter);
        return shelter.Count();
    }

    public object HandleList(Request request) {
        return service.ListAnimals();
    }

    public bool Validate(Request request) {
        if (request.Get("name") == null) {
            throw new ArgumentException("name required");
        }
        return true;
    }
}
