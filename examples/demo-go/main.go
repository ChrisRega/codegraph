// Tiny entry point so `go build` works on the demo.
package main

import (
	"fmt"

	"example.com/demoapp/greet"
	"example.com/demoapp/mathops"
)

func main() {
	fmt.Println(greet.Shout("world"))
	fmt.Println(mathops.Double(7))
}
