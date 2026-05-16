"""Greeting helpers — mirrors the Rust + TypeScript demos."""


def hello(name: str) -> str:
    """Return a friendly greeting."""
    return f"hello, {name}"


def shout(name: str) -> str:
    """Return :func:`hello` in upper case."""
    return hello(name).upper()
