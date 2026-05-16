// Package greet exposes hello + shout — mirrors the Rust / Python / TS demos.
package greet

import "strings"

// Hello returns a friendly greeting.
func Hello(name string) string {
	return "hello, " + name
}

// Shout returns Hello in upper case.
func Shout(name string) string {
	return strings.ToUpper(Hello(name))
}
