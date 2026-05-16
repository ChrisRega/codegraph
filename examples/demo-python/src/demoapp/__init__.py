"""Tiny demo package used by the codegraph indexer integration tests."""

from .greet import hello, shout
from .math_ops import add, double

__all__ = ["hello", "shout", "add", "double"]
