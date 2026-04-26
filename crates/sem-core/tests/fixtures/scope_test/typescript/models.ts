export class Dog {
    private name: string;

    constructor(name: string) {
        this.name = name;
    }

    speak(): string {
        return "woof";
    }

    validate(): boolean {
        return this.name.length > 0;
    }
}

export class Cat {
    private name: string;

    constructor(name: string) {
        this.name = name;
    }

    speak(): string {
        return "meow";
    }

    validate(): boolean {
        return this.name.length > 0 && this.name.length < 50;
    }
}

export class Shelter {
    private animals: any[];

    constructor() {
        this.animals = [];
    }

    add(animal: any): void {
        if (animal.validate()) {
            this.animals.push(animal);
        }
    }

    count(): number {
        return this.animals.length;
    }
}
