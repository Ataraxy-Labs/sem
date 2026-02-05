export function greet(name: string): string {
  return `Hello, ${name}!`;
}

export function farewell(name: string): string {
  return `Goodbye, ${name}!`;
}

export class Calculator {
  add(a: number, b: number): number {
    return a + b;
  }
}
