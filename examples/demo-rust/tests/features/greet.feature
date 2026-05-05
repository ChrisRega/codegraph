Feature: Greeting

  Scenario: friendly hello
    Given a name "world"
    When I call hello
    Then the result is "hello, world"

  Scenario: shouted hello
    Given a name "world"
    When I call shout
    Then the result is "HELLO, WORLD"
