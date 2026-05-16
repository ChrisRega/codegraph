package mathops

import "testing"

func TestAdd(t *testing.T) {
	if got := Add(2, 3); got != 5 {
		t.Fatalf("Add: got %d, want 5", got)
	}
}

func TestDoubleCallsAdd(t *testing.T) {
	if got := Double(7); got != 14 {
		t.Fatalf("Double: got %d, want 14", got)
	}
}
