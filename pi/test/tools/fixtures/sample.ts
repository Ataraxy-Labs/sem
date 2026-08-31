export class Alice {
  greet(): string {
    return "hi from alice";
  }
}

export class Bob {
  greet(): string {
    return "hi from bob";
  }
}

export function standalone(): number {
  return 1;
}

export function helper(): number {
  return 2;
}
