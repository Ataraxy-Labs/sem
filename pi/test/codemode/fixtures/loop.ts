export function loopA(): number {
  return loopB();
}

export function loopB(): number {
  return loopC();
}

export function loopC(): number {
  return loopA();
}
