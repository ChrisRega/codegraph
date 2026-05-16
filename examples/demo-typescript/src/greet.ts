// Greeting helpers — mirrors the Rust + Python demos.

export function hello(name: string): string {
  return `hello, ${name}`;
}

export function shout(name: string): string {
  return hello(name).toUpperCase();
}
