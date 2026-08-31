import { add, sub } from "./math.ts";

export function calculate(a: number, b: number): number {
  return add(a, b) + sub(a, b);
}
