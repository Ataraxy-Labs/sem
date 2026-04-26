import { createDog, createCat, transferAnimal, listAnimals } from './service';
import { Dog, Cat, Shelter } from './models';

export function handleCreateDog(request: any): any {
    const name = request.name;
    return createDog(name);
}

export function handleCreateCat(request: any): any {
    const name = request.name;
    return createCat(name);
}

export function handleTransfer(request: any): number {
    const shelter = new Shelter();
    const dog = new Dog(request.name);
    transferAnimal(dog, shelter);
    return shelter.count();
}

export function handleList(request: any): any[] {
    const animals = listAnimals();
    return animals;
}

export function validate(request: any): boolean {
    if (!request.name) {
        throw new Error("name required");
    }
    return true;
}
