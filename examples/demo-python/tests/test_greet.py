from demoapp.greet import hello, shout


def test_hello() -> None:
    assert hello("world") == "hello, world"


def test_shout_uppercases_hello() -> None:
    assert shout("world") == "HELLO, WORLD"
