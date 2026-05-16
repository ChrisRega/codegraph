package greet

import "testing"

func TestHello(t *testing.T) {
	if got := Hello("world"); got != "hello, world" {
		t.Fatalf("Hello: got %q, want %q", got, "hello, world")
	}
}

func TestShoutUppercasesHello(t *testing.T) {
	if got := Shout("world"); got != "HELLO, WORLD" {
		t.Fatalf("Shout: got %q, want %q", got, "HELLO, WORLD")
	}
}
