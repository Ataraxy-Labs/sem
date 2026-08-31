export function add(a: number, b: number): number {
  return a + b;
}

// Doubles a number by adding it to itself.
export function twice(n: number): number {
  return add(n, n);
}

export function thrice(n: number): number {
  return add(n, n) + n;
}
