// Package mathops exposes add + double — mirrors the Rust / Python / TS demos.
package mathops

// Add returns the sum of two integers.
func Add(a, b int) int {
	return a + b
}

// Double doubles x by delegating to Add.
func Double(x int) int {
	return Add(x, x)
}
