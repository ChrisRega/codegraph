import { describe, it, expect } from "vitest";
import { hello, shout } from "../src/greet.js";

describe("greet", () => {
  it("hello returns a friendly greeting", () => {
    expect(hello("world")).toBe("hello, world");
  });

  it("shout uppercases hello", () => {
    expect(shout("world")).toBe("HELLO, WORLD");
  });
});
