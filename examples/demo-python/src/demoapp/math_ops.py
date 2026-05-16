"""Trivial arithmetic helpers — mirrors the Rust + TypeScript demos."""


def add(a: int, b: int) -> int:
    return a + b


def double(x: int) -> int:
    return add(x, x)
