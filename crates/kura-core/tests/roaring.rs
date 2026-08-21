//! What a bitmap written by this build looks like to somebody else.
//!
//! A round trip test passes just as happily on a private format that resembles
//! the specification, and the whole reason for writing a bitmap in the portable
//! Roaring layout rather than in whatever this crate finds convenient is that
//! another program reads it. So the fixtures in `testdata/roaring` were not
//! written by this build. They were written by the reference Go library, at the
//! version named in `testdata/roaring/reference.go`, which also has the command
//! that regenerates them.
//!
//! The two halves fail for different reasons.
//!
//! Reading the fixtures catches this build not understanding a bitmap that the
//! rest of the world calls valid, including one it would never have produced
//! itself, such as a container shape chosen differently or an offset table that
//! is present when this build would have left it out.
//!
//! Writing them again and comparing byte for byte catches the other direction,
//! which is this build producing something that only this build can read. That
//! is the failure with no symptom until somebody else is holding the file, and
//! it is the one that a decoder written to match the encoder cannot see.
//!
//! # When it fails
//!
//! The answer is not to write the fixture again from this build. It is to work
//! out which of the two is wrong against the format, which is documented in the
//! specification the reference library implements, and to fix that side.

use kura_core::DocId;
use kura_core::bitmap::Bitmap;

/// Where the fixtures live, from wherever the test binary is run.
fn testdata() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/roaring")
}

fn step(from: DocId, to: DocId, by: DocId) -> Vec<DocId> {
    (from..to).step_by(by as usize).collect()
}

fn shift(by: DocId, ordinals: &[DocId]) -> Vec<DocId> {
    ordinals.iter().map(|&low| by + low).collect()
}

/// The sets the fixtures hold, spelled out here and again in `reference.go`.
///
/// Saying it twice is the point. If the Rust side took the set from the file it
/// is checking, the file could hold anything and the test would agree with it.
fn fixtures() -> Vec<(&'static str, Vec<DocId>)> {
    let sparse = vec![0, 1, 9, 63, 64, 4096];
    let run = step(0, 1000, 1);
    let scattered = step(0, 30000, 3);

    let mut mixed = sparse.clone();
    mixed.extend(shift(1 << 16, &run));
    mixed.extend(shift(2 << 16, &scattered));

    let mut many = Vec::new();
    for (at, key) in [0u32, 1, 2, 7, 9].into_iter().enumerate() {
        let part = if at % 2 == 0 { &sparse } else { &run };
        many.extend(shift(key << 16, part));
    }

    vec![
        // Nothing at all, which is the one shape that cannot carry a container
        // count in the cookie and so has to be written the other way.
        ("empty", Vec::new()),
        ("one", vec![7]),
        // An array container, and a member at the far end of the chunk.
        ("sparse", sparse),
        // A single run, in a bitmap too small to carry the offsets.
        ("run", run),
        // Too scattered to be either of the other two, so a word block.
        ("scattered", scattered),
        // Exactly as many members as an array container is allowed to hold,
        // which is where the reader stops believing the shape and starts
        // deciding it from the count.
        ("boundary", step(0, 8192, 2)),
        // A whole chunk in one run, where the count and the run length are both
        // one larger than the two bytes they are written in can hold.
        ("full", step(0, 65536, 1)),
        // The first and last members of the space, and the seam between two
        // chunks.
        ("edges", vec![0, 65535, 65536, u32::MAX - 1, u32::MAX]),
        // All three shapes in one bitmap, still under the container count that
        // brings the offsets back.
        ("mixed", mixed),
        // Over it, with the keys not consecutive, so the offsets are written
        // and the chunk a container belongs to cannot be inferred from where it
        // sits.
        ("many", many),
    ]
}

#[test]
fn what_the_reference_wrote_reads_back_as_the_set_it_was_given() {
    for (name, ordinals) in fixtures() {
        let bytes = std::fs::read(testdata().join(format!("{name}.bin")))
            .unwrap_or_else(|_| panic!("the {name} fixture is there"));
        let map = Bitmap::read(&bytes).unwrap_or_else(|error| panic!("{name} reads: {error}"));
        assert_eq!(map.len(), ordinals.len(), "{name} holds a different count");
        assert_eq!(map.to_vec(), ordinals, "{name} holds different members");
        for ordinal in &ordinals {
            assert!(map.contains(*ordinal), "{name} lost {ordinal}");
        }
    }
}

#[test]
fn what_this_build_writes_is_what_the_reference_wrote() {
    for (name, ordinals) in fixtures() {
        let map = Bitmap::from_sorted(&ordinals);
        let mut ours = Vec::new();
        map.write_to(&mut ours);
        let theirs = std::fs::read(testdata().join(format!("{name}.bin")))
            .unwrap_or_else(|_| panic!("the {name} fixture is there"));
        assert_eq!(
            ours.len(),
            theirs.len(),
            "{name} is {} bytes here and {} bytes there",
            ours.len(),
            theirs.len()
        );
        assert_eq!(ours, theirs, "{name} differs from the reference");
        assert_eq!(map.size(), theirs.len(), "{name} misreports its size");
    }
}

#[test]
fn a_bitmap_built_by_inserting_is_written_the_same_way_as_one_built_from_a_slice() {
    // The shape a chunk is held in depends on how it was reached, and the shape
    // it is written in must not. A set inserted one member at a time can be
    // holding a word block where the same set built from a sorted slice is
    // holding runs, and the bytes have to come out the same either way.
    for (name, ordinals) in fixtures() {
        let mut inserted = Bitmap::new();
        for ordinal in &ordinals {
            inserted.insert(*ordinal);
        }
        let mut ours = Vec::new();
        inserted.write_to(&mut ours);
        let theirs = std::fs::read(testdata().join(format!("{name}.bin")))
            .unwrap_or_else(|_| panic!("the {name} fixture is there"));
        assert_eq!(
            ours, theirs,
            "{name} differs when it was built by inserting"
        );
    }
}
