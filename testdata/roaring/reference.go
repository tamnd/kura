// Writes the bitmap fixtures beside this file with another implementation.
//
// The Rust tests in crates/kura-core/tests/roaring.rs read these files and
// write them again, so what they are checking is that this build and the
// reference library agree on the bytes rather than that this build agrees with
// itself. Regenerate them with a Go toolchain and the version below:
//
//	go mod init reference
//	go get github.com/RoaringBitmap/roaring/v2@v2.25.0
//	go run reference.go
//
// The sets are spelled out here and again in the Rust test, on purpose. A
// fixture is only worth anything if both sides say what the set is and then
// disagree about the bytes.
package main

import (
	"fmt"
	"os"

	"github.com/RoaringBitmap/roaring/v2"
)

func write(name string, ordinals []uint32) {
	b := roaring.New()
	for _, o := range ordinals {
		b.Add(o)
	}
	// The library writes array and bitmap containers and nothing else until it
	// is asked, and a run container is the shape kura reaches for whenever it
	// is the smallest of the three.
	b.RunOptimize()
	bytes, err := b.ToBytes()
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(name+".bin", bytes, 0o644); err != nil {
		panic(err)
	}
	fmt.Printf("%-10s %6d bytes %5d members\n", name, len(bytes), b.GetCardinality())
}

func step(from, to, by uint32) []uint32 {
	var out []uint32
	for i := from; i < to; i += by {
		out = append(out, i)
	}
	return out
}

func shift(by uint32, ordinals []uint32) []uint32 {
	out := make([]uint32, 0, len(ordinals))
	for _, o := range ordinals {
		out = append(out, by+o)
	}
	return out
}

func main() {
	sparse := []uint32{0, 1, 9, 63, 64, 4096}
	run := step(0, 1000, 1)
	scattered := step(0, 30000, 3)

	write("empty", nil)
	write("one", []uint32{7})
	write("sparse", sparse)
	write("run", run)
	write("scattered", scattered)
	write("boundary", step(0, 8192, 2))
	write("full", step(0, 65536, 1))
	write("edges", []uint32{0, 65535, 65536, 4294967294, 4294967295})

	var mixed []uint32
	mixed = append(mixed, sparse...)
	mixed = append(mixed, shift(1<<16, run)...)
	mixed = append(mixed, shift(2<<16, scattered)...)
	write("mixed", mixed)

	var many []uint32
	for i, key := range []uint32{0, 1, 2, 7, 9} {
		if i%2 == 0 {
			many = append(many, shift(key<<16, sparse)...)
		} else {
			many = append(many, shift(key<<16, run)...)
		}
	}
	write("many", many)
}
