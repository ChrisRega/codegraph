import { describe, it, expect } from "vitest";
import { add, double } from "../src/math.js";

describe("math", () => {
  it("add returns the sum", () => {
    expect(add(2, 3)).toBe(5);
  });

  it("double calls add", () => {
    expect(double(7)).toBe(14);
  });
});
