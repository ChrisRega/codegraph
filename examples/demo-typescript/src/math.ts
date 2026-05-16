// Trivial arithmetic helpers — mirrors the Rust + Python demos.

export function add(a: number, b: number): number {
  return a + b;
}

export function double(x: number): number {
  return add(x, x);
}
