from demoapp.math_ops import add, double


def test_add() -> None:
    assert add(2, 3) == 5


def test_double_calls_add() -> None:
    assert double(7) == 14
