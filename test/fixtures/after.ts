export function greet(name: string, formal?: boolean): string {
  return formal ? `Good day, ${name}.` : `Hello, ${name}!`;
}

export function welcome(name: string): string {
  return `Welcome, ${name}!`;
}

export class Calculator {
  add(a: number, b: number): number {
    return a + b;
  }

  subtract(a: number, b: number): number {
    return a - b;
  }
}
