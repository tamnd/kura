//! Reading a file without copying it.
//!
//! Everything in this crate that reads takes a byte slice, which leaves whoever
//! opens the file free to decide where the bytes come from. For a while the
//! answer in the command line tool was `fs::read` and the answer in the
//! benchmark runner was a mapping. Those are not the same program. A 700 MB
//! index costs 700 MB of resident memory and a full read one way, and a few
//! pages and no read the other, and the results table said kura for both.
//!
//! So this maps. A query touches the dictionary, one skip table per term and
//! the blocks it does not step over, and on a large index that is a small
//! fraction of the file. Reading all of it to look at some of it is work that
//! nothing asked for.
//!
//! # Why it is in the engine
//!
//! It started in the tool, which was the wrong place as soon as [`file`] needed
//! it too. Two implementations of two platforms in two crates is the drift this
//! crate already had once over fault counters, and the fix was the same then:
//! one of them, here, used by everything.
//!
//! # Why it is written out by hand
//!
//! Three declarations and a destructor, against a dependency in a crate that
//! has none. The engine has no dependencies because it gets linked into other
//! people's binaries, and this is small enough not to be the exception.
//!
//! # What can still go wrong
//!
//! A mapping is a window onto a file rather than a copy of it, so a file that
//! is truncated while it is open takes the process down with a bus error on the
//! next touch past the new end. That is the price of not copying and every
//! engine that maps pays it. A store grows at the end and is never shortened
//! underneath a reader, so this is a note rather than a hazard, and it is why
//! the mapping is read only.
//!
//! [`file`]: crate::file

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::path::Path;

/// A read only view of a file.
///
/// Derefs to the bytes, so it goes straight into anything that takes a slice,
/// and unmaps when it is dropped. The bytes live as long as the value does,
/// which is what keeps a slice handed out of here from outliving the mapping it
/// points into.
pub struct Map {
    /// Where the mapping starts, or a dangling pointer when `len` is zero.
    address: *const u8,
    /// How many bytes are mapped.
    len: usize,
}

impl Map {
    /// Maps the whole of a file, given where it is.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened, cannot be measured, or
    /// cannot be mapped.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::of(&File::open(path)?)
    }

    /// Maps the whole of a file that is already open.
    ///
    /// The mapping does not keep the descriptor and does not need to. A mapping
    /// holds a reference of its own to the file on both platforms here, so the
    /// caller is free to close the one it passed in, and a store that has its
    /// file open for writing can hand it over without opening the path a second
    /// time and racing whatever is at that path by then.
    ///
    /// The mapping is read only regardless of what the descriptor was opened
    /// for, which is why a store can do this to the file it is also writing to.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be measured or cannot be mapped.
    pub fn of(file: &File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        let len = usize::try_from(len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the file is larger than this machine can address",
            )
        })?;

        // An empty file is not a mapping. Both platforms refuse a length of
        // zero, and the error they give back describes an invalid argument
        // rather than an empty file, which sends the reader looking in the
        // wrong place. An empty slice reaches the format check instead, and
        // that says the magic is missing, which is what is actually wrong.
        if len == 0 {
            return Ok(Self {
                address: std::ptr::NonNull::dangling().as_ptr(),
                len: 0,
            });
        }

        platform::map(file, len)
    }
}

impl Deref for Map {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `address` and `len` came from a successful mapping of that
        // length and are only ever written by `of`, the region is mapped for
        // as long as `self` lives because `drop` is the only thing that unmaps
        // it, and the borrow returned cannot outlive `self`. When `len` is zero
        // the pointer is dangling but aligned, which is what an empty slice
        // requires.
        unsafe { std::slice::from_raw_parts(self.address, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        if self.len == 0 {
            return;
        }
        platform::unmap(self);
    }
}

impl std::fmt::Debug for Map {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The address and the length, and not what is at the address. A derived
        // Debug on something that derefs to a slice prints the whole index.
        f.debug_struct("Map")
            .field("address", &self.address)
            .field("len", &self.len)
            .finish()
    }
}

// SAFETY: the mapping is read only and the value owns it outright, so there is
// no interior mutability and nothing to share with the thread it came from. The
// only operations are a read of two fields and, once, an unmap of a region
// nothing else refers to.
unsafe impl Send for Map {}
// SAFETY: as above. Every shared reference can do is read the bytes, and the
// bytes never change.
unsafe impl Sync for Map {}

#[cfg(unix)]
mod platform {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd as _;

    use super::Map;

    /// Read access, which is all this ever asks for.
    const PROT_READ: i32 = 1;

    /// A private mapping, so nothing written through it could reach the file.
    ///
    /// Nothing writes through it, since the protection above does not allow it.
    /// This is the belt to that pair of braces, and it is also what every other
    /// reader of a file it does not own uses.
    const MAP_PRIVATE: i32 = 2;

    /// What `mmap` returns when it fails, which is not a null pointer.
    const MAP_FAILED: isize = -1;

    // SAFETY: the signatures match the platform's, which is the same on every
    // Unix we build for. `off_t` is 64 bits on all of them, since the 32 bit
    // targets that could disagree are not ones this tool is built for.
    unsafe extern "C" {
        fn mmap(
            addr: *mut core::ffi::c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut core::ffi::c_void;

        fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
    }

    /// Maps `len` bytes from the start of `file`.
    pub fn map(file: &File, len: usize) -> io::Result<Map> {
        // SAFETY: a null address asks the kernel to choose one, the descriptor
        // is open for reading and outlives the call, the length is the file's
        // own and is not zero, and the offset is the start of the file.
        let address = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };

        if address as isize == MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(Map {
            address: address.cast::<u8>(),
            len,
        })
    }

    /// Releases the mapping.
    pub fn unmap(map: &Map) {
        // SAFETY: the address and length are the ones the mapping was made
        // with, and this runs once, from `drop`, on a value nothing else can
        // reach any more.
        //
        // The result is ignored because there is nothing to do about it. The
        // only reasons this fails are a wrong address or a wrong length, both
        // of which would be bugs here rather than conditions, and a destructor
        // is the worst place to find out.
        unsafe {
            munmap(map.address.cast::<core::ffi::c_void>().cast_mut(), map.len);
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle as _;

    use super::Map;

    /// A mapping whose pages can be read and not written.
    const PAGE_READONLY: u32 = 0x02;

    /// A view that can be read and not written.
    const FILE_MAP_READ: u32 = 4;

    // SAFETY: the signatures match the ones in the platform headers. The
    // library is named explicitly rather than relied on to be linked already,
    // so that this does not depend on what the standard library happens to
    // pull in.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileMappingW(
            file: *mut core::ffi::c_void,
            attributes: *mut core::ffi::c_void,
            protect: u32,
            maximum_size_high: u32,
            maximum_size_low: u32,
            name: *const u16,
        ) -> *mut core::ffi::c_void;

        fn MapViewOfFile(
            mapping: *mut core::ffi::c_void,
            access: u32,
            offset_high: u32,
            offset_low: u32,
            bytes: usize,
        ) -> *mut core::ffi::c_void;

        fn UnmapViewOfFile(address: *const core::ffi::c_void) -> i32;

        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
    }

    /// Maps `len` bytes from the start of `file`.
    ///
    /// Two calls rather than one, because Windows separates the mapping object
    /// from the view of it. The mapping object is closed as soon as the view
    /// exists: the view holds its own reference, so the region stays mapped,
    /// and there is then one handle to get wrong instead of two.
    pub fn map(file: &File, len: usize) -> io::Result<Map> {
        // SAFETY: the file handle is open for reading and outlives the call,
        // the default attributes and no name are what a null asks for, and a
        // maximum size of zero means the whole file.
        let mapping = unsafe {
            CreateFileMappingW(
                file.as_raw_handle().cast::<core::ffi::c_void>(),
                std::ptr::null_mut(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            )
        };
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: the mapping handle is the one just returned and is still
        // open, and the offset is the start of the file.
        let address = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, len) };

        // Closed whether or not the view was made. On success the view keeps
        // the region alive on its own, and on failure there is nothing to keep
        // alive, so the only path where holding on to this would be right is
        // one where the view is never unmapped.
        //
        // The error is taken before this, because CloseHandle succeeding would
        // otherwise overwrite the reason MapViewOfFile failed.
        let failure = if address.is_null() {
            Some(io::Error::last_os_error())
        } else {
            None
        };
        // SAFETY: the handle is the one returned above, it is open, and this
        // is the only place it is closed.
        unsafe {
            CloseHandle(mapping);
        }
        if let Some(failure) = failure {
            return Err(failure);
        }

        Ok(Map {
            address: address.cast::<u8>(),
            len,
        })
    }

    /// Releases the view.
    pub fn unmap(map: &Map) {
        // SAFETY: the address is the one the view was made at, and this runs
        // once, from `drop`, on a value nothing else can reach any more.
        //
        // The result is ignored for the reason given on the Unix side: the
        // failures are bugs rather than conditions, and a destructor cannot act
        // on either.
        unsafe {
            UnmapViewOfFile(map.address.cast::<core::ffi::c_void>());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A file with `content` in it, under a name nothing else in this run uses.
    fn written(name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("kura-map-{}-{name}", std::process::id()));
        let mut file = std::fs::File::create(&path).expect("the temporary directory is writable");
        file.write_all(content).expect("a few bytes fit");
        file.sync_all().expect("the bytes reach the file");
        path
    }

    #[test]
    fn a_mapped_file_reads_back_as_what_was_written() {
        let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let path = written("round-trip", &content);
        let map = Map::open(&path).expect("a file that exists maps");
        assert_eq!(&map[..], &content[..]);
        drop(map);
        std::fs::remove_file(&path).expect("the file is still there");
    }

    #[test]
    fn an_empty_file_maps_to_an_empty_slice_rather_than_failing() {
        // The interesting case, because both platforms refuse a mapping of
        // length zero and the error they give describes an invalid argument.
        // A caller that gets that instead of an empty slice is told the wrong
        // thing about a file that is merely empty.
        let path = written("empty", b"");
        let map = Map::open(&path).expect("an empty file is not an error");
        assert!(map.is_empty());
        drop(map);
        std::fs::remove_file(&path).expect("the file is still there");
    }

    #[test]
    fn a_file_that_is_not_there_is_an_error_and_not_a_panic() {
        let path = std::env::temp_dir().join("kura-map-absent-on-purpose");
        let _ = std::fs::remove_file(&path);
        assert!(Map::open(&path).is_err());
    }

    #[test]
    fn the_bytes_survive_the_file_being_unlinked() {
        // Which is the property that makes this safe to use for the length of a
        // command. On Unix the file lives until the last reference goes, and on
        // Windows the mapping holds it open, so a caller does not have to keep
        // the directory entry alive to keep reading.
        let path = written("unlinked", b"kura");
        let map = Map::open(&path).expect("maps");
        let _ = std::fs::remove_file(&path);
        assert_eq!(&map[..], b"kura");
    }

    #[test]
    fn debug_does_not_print_the_contents() {
        // A derived Debug on something that derefs to a slice prints the whole
        // index, which in an error message is a few hundred megabytes of binary
        // down somebody's terminal.
        let path = written("debug", b"0123456789");
        let map = Map::open(&path).expect("maps");
        let text = format!("{map:?}");
        assert!(text.contains("len: 10"), "{text}");
        // A bracket rather than a byte value. Looking for the decimal of one of
        // the bytes finds it in the address about one run in twenty, because an
        // address is printed in hexadecimal and hexadecimal is mostly digits.
        assert!(!text.contains('['), "{text}");
        drop(map);
        std::fs::remove_file(&path).expect("the file is still there");
    }
}
