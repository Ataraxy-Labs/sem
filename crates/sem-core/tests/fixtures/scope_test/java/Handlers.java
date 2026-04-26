public class Handlers {
    private Service service = new Service();

    public Dog handleCreateDog(Request request) {
        String name = request.get("name");
        return service.createDog(name);
    }

    public Cat handleCreateCat(Request request) {
        String name = request.get("name");
        return service.createCat(name);
    }

    public int handleTransfer(Request request) {
        Shelter shelter = new Shelter();
        Dog dog = new Dog(request.get("name"));
        service.transferAnimal(dog, shelter);
        return shelter.count();
    }

    public Object handleList(Request request) {
        return service.listAnimals();
    }

    public boolean validate(Request request) {
        if (request.get("name") == null) {
            throw new IllegalArgumentException("name required");
        }
        return true;
    }
}
