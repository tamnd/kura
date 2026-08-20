//! The C ABI of the storage engine.
//!
//! This crate exists so that a host process written in another language can use
//! the engine without a server, a socket or a copy of the data. It is built as a
//! static library and a shared library, and the matching header is checked in at
//! `include/kura.h`.
//!
//! Three rules hold everywhere in this file.
//!
//! Every function returns a status code, never a value that could also be a
//! valid result. Results come back through out parameters. A caller that ignores
//! the status gets a zeroed out parameter rather than a plausible wrong answer.
//!
//! Every pointer is checked before it is read. Passing null is a supported way to
//! get an error rather than a crash, because the caller is usually a garbage
//! collected runtime where a nil slice is ordinary.
//!
//! No panic crosses the boundary. Unwinding into foreign frames is undefined
//! behaviour, so every entry point catches first and reports [`KURA_ERR_PANIC`].
//!
//! Memory that the engine allocates is freed by the engine. Anything returned in
//! a [`KuraBuffer`] goes back through [`kura_buffer_free`], and every handle has
//! its own free function.

use core::ffi::{CStr, c_char};
use core::panic::AssertUnwindSafe;
use core::slice;
use std::panic::catch_unwind;

use kura_core::bitmap::Bitmap;
use kura_core::error::Error;
use kura_core::posting::{Reader, Writer};
use kura_core::vector;

/// The call succeeded.
pub const KURA_OK: i32 = 0;
/// A required pointer was null.
pub const KURA_ERR_NULL: i32 = 1;
/// The input ended in the middle of a value.
pub const KURA_ERR_TRUNCATED: i32 = 2;
/// A variable length integer did not terminate.
pub const KURA_ERR_OVERFLOW: i32 = 3;
/// The input does not start with this engine's magic bytes.
pub const KURA_ERR_BAD_MAGIC: i32 = 4;
/// The format version is one this build does not read.
pub const KURA_ERR_UNSUPPORTED_VERSION: i32 = 5;
/// A checksum did not match.
pub const KURA_ERR_CHECKSUM: i32 = 6;
/// Two vectors of different lengths were compared.
pub const KURA_ERR_DIMENSION_MISMATCH: i32 = 7;
/// Document ids were not in ascending order.
pub const KURA_ERR_NOT_SORTED: i32 = 8;
/// The caller's buffer is too small for the result.
pub const KURA_ERR_BUFFER_TOO_SMALL: i32 = 9;
/// The engine panicked and the call was abandoned.
pub const KURA_ERR_PANIC: i32 = 10;

/// The version of this ABI.
///
/// It changes whenever a signature, a status code or a struct layout changes. A
/// host that links a prebuilt library should compare it against the value its
/// header was generated from before calling anything else.
pub const KURA_ABI_VERSION: u32 = 2;

/// A block of bytes the engine allocated.
///
/// The layout is part of the ABI. It has to be returned to [`kura_buffer_free`]
/// unchanged, because the allocator needs the capacity as well as the length.
#[repr(C)]
#[derive(Debug)]
pub struct KuraBuffer {
    /// The first byte, or null if the buffer is empty.
    pub data: *mut u8,
    /// How many bytes are used.
    pub len: usize,
    /// How many bytes were allocated.
    pub cap: usize,
}

impl KuraBuffer {
    /// An empty buffer, which is what an out parameter holds after a failure.
    const fn empty() -> Self {
        Self {
            data: core::ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }

    fn from_vec(mut bytes: Vec<u8>) -> Self {
        let buffer = Self {
            data: bytes.as_mut_ptr(),
            len: bytes.len(),
            cap: bytes.capacity(),
        };
        core::mem::forget(bytes);
        buffer
    }
}

/// A set of document ids.
///
/// The contents are opaque on purpose. The representation switches between a
/// sorted list and a dense word array depending on how full the set is, and a
/// caller that depended on either one would break the first time the other was
/// chosen.
#[derive(Debug)]
pub struct KuraBitmap {
    inner: Bitmap,
}

/// Returns the ABI version this library was built with.
#[unsafe(no_mangle)]
pub extern "C" fn kura_abi_version() -> u32 {
    KURA_ABI_VERSION
}

/// Returns the crate version as a null terminated string.
///
/// The string is static and must not be freed.
#[unsafe(no_mangle)]
pub extern "C" fn kura_version() -> *const c_char {
    const VERSION: &CStr =
        match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
            Ok(v) => v,
            Err(_) => c"unknown",
        };
    VERSION.as_ptr()
}

/// Returns a static description of a status code.
///
/// The string must not be freed. An unknown code gives a generic message rather
/// than null, so that a caller can always print something.
#[unsafe(no_mangle)]
pub extern "C" fn kura_status_message(status: i32) -> *const c_char {
    let message: &CStr = match status {
        KURA_OK => c"ok",
        KURA_ERR_NULL => c"a required pointer was null",
        KURA_ERR_TRUNCATED => c"input ended early",
        KURA_ERR_OVERFLOW => c"variable length integer did not terminate",
        KURA_ERR_BAD_MAGIC => c"not a kura file",
        KURA_ERR_UNSUPPORTED_VERSION => c"unsupported format version",
        KURA_ERR_CHECKSUM => c"checksum mismatch",
        KURA_ERR_DIMENSION_MISMATCH => c"vectors of different lengths",
        KURA_ERR_NOT_SORTED => c"document ids are not ascending",
        KURA_ERR_BUFFER_TOO_SMALL => c"the buffer is too small for the result",
        KURA_ERR_PANIC => c"the engine abandoned the call",
        _ => c"unknown status",
    };
    message.as_ptr()
}

/// Frees a buffer the engine returned.
///
/// Passing a buffer with a null pointer is a no op, so a caller can free the out
/// parameter of a failed call without checking it first.
///
/// # Safety
///
/// The buffer must be one the engine returned, unmodified, and it must not be
/// freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_buffer_free(buffer: KuraBuffer) {
    if buffer.data.is_null() {
        return;
    }
    // SAFETY: the contract above says the three fields are the ones from_vec
    // wrote, which came from a Vec<u8> that was forgotten rather than dropped.
    drop(unsafe { Vec::from_raw_parts(buffer.data, buffer.len, buffer.cap) });
}

/// Creates an empty set of document ids.
///
/// Returns null only if the allocation failed.
#[unsafe(no_mangle)]
pub extern "C" fn kura_bitmap_new() -> *mut KuraBitmap {
    guard_ptr(|| {
        Box::into_raw(Box::new(KuraBitmap {
            inner: Bitmap::new(),
        }))
    })
}

/// Frees a set of document ids. Passing null is a no op.
///
/// # Safety
///
/// The handle must have come from [`kura_bitmap_new`] and must not be freed
/// twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_bitmap_free(bitmap: *mut KuraBitmap) {
    if bitmap.is_null() {
        return;
    }
    // SAFETY: the contract above says the pointer came from Box::into_raw and
    // has not been freed yet.
    drop(unsafe { Box::from_raw(bitmap) });
}

/// Adds a document id.
///
/// # Safety
///
/// `bitmap` must be a live handle from [`kura_bitmap_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_bitmap_insert(bitmap: *mut KuraBitmap, id: u32) -> i32 {
    if bitmap.is_null() {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: checked for null above, and the contract says the handle is
        // live and not aliased for the duration of the call.
        let bitmap = unsafe { &mut *bitmap };
        bitmap.inner.insert(id);
        KURA_OK
    })
}

/// Removes a document id.
///
/// # Safety
///
/// `bitmap` must be a live handle from [`kura_bitmap_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_bitmap_remove(bitmap: *mut KuraBitmap, id: u32) -> i32 {
    if bitmap.is_null() {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: checked for null above, and the contract says the handle is
        // live and not aliased for the duration of the call.
        let bitmap = unsafe { &mut *bitmap };
        bitmap.inner.remove(id);
        KURA_OK
    })
}

/// Writes 1 or 0 into `out` depending on whether `id` is in the set.
///
/// # Safety
///
/// `bitmap` must be a live handle and `out` must point at a writable `int`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_bitmap_contains(
    bitmap: *const KuraBitmap,
    id: u32,
    out: *mut i32,
) -> i32 {
    if bitmap.is_null() || out.is_null() {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: both pointers were checked for null, and the contract says
        // they are live for the duration of the call.
        unsafe {
            let found = (*bitmap).inner.contains(id);
            out.write(i32::from(found));
        }
        KURA_OK
    })
}

/// Writes how many document ids the set holds into `out`.
///
/// # Safety
///
/// `bitmap` must be a live handle and `out` must point at a writable `size_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_bitmap_len(bitmap: *const KuraBitmap, out: *mut usize) -> i32 {
    if bitmap.is_null() || out.is_null() {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: both pointers were checked for null, and the contract says
        // they are live for the duration of the call.
        unsafe {
            let len = (*bitmap).inner.len();
            out.write(len);
        }
        KURA_OK
    })
}

/// Keeps only the ids that are in both sets, leaving the result in `bitmap`.
///
/// This is the operation a permission filter runs, which is why it is on the
/// boundary at all: a host that had to move the ids out and back would spend
/// more time copying than the intersection itself takes.
///
/// # Safety
///
/// Both handles must be live, and they must not be the same handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_bitmap_intersect(
    bitmap: *mut KuraBitmap,
    other: *const KuraBitmap,
) -> i32 {
    if bitmap.is_null() || other.is_null() {
        return KURA_ERR_NULL;
    }
    if bitmap.cast_const() == other {
        // Intersecting a set with itself is a no op, and doing it through two
        // references to the same allocation is not.
        return KURA_OK;
    }
    guard(|| {
        // SAFETY: both pointers were checked for null and for aliasing, and the
        // contract says they are live for the duration of the call.
        unsafe {
            let other = &(*other).inner;
            (*bitmap).inner.intersect_with(other);
        }
        KURA_OK
    })
}

/// Adds every id from `other` to `bitmap`.
///
/// # Safety
///
/// Both handles must be live, and they must not be the same handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_bitmap_union(
    bitmap: *mut KuraBitmap,
    other: *const KuraBitmap,
) -> i32 {
    if bitmap.is_null() || other.is_null() {
        return KURA_ERR_NULL;
    }
    if bitmap.cast_const() == other {
        return KURA_OK;
    }
    guard(|| {
        // SAFETY: both pointers were checked for null and for aliasing, and the
        // contract says they are live for the duration of the call.
        unsafe {
            let other = &(*other).inner;
            (*bitmap).inner.union_with(other);
        }
        KURA_OK
    })
}

/// Copies the ids into `out`, in ascending order.
///
/// `out_len` always receives the number of ids in the set, so a caller that
/// passes a capacity of zero learns how much to allocate and gets
/// [`KURA_ERR_BUFFER_TOO_SMALL`] back.
///
/// # Safety
///
/// `bitmap` must be a live handle, `out` must be writable for `cap` ids, and
/// `out_len` must point at a writable `size_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_bitmap_to_array(
    bitmap: *const KuraBitmap,
    out: *mut u32,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if bitmap.is_null() || out_len.is_null() {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: both pointers were checked for null, and the contract says
        // they are live for the duration of the call.
        let ids = unsafe { (*bitmap).inner.to_vec() };
        // SAFETY: out_len was checked for null above.
        unsafe { out_len.write(ids.len()) };

        if ids.len() > cap {
            return KURA_ERR_BUFFER_TOO_SMALL;
        }
        if ids.is_empty() {
            return KURA_OK;
        }
        if out.is_null() {
            return KURA_ERR_NULL;
        }
        // SAFETY: out is non null and the contract says it is writable for cap
        // ids, which the check above proved is at least ids.len().
        unsafe { core::ptr::copy_nonoverlapping(ids.as_ptr(), out, ids.len()) };
        KURA_OK
    })
}

/// Encodes ascending document ids into a compressed posting list.
///
/// `frequencies` says how often the term occurs in each document and may be
/// null, which means once in each. A caller building a plain set of documents
/// rather than a scored index wants null and should not have to allocate an
/// array of ones to say so.
///
/// On success `out` receives a buffer that has to go back to
/// [`kura_buffer_free`]. On failure it receives an empty buffer, which is safe
/// to free.
///
/// # Safety
///
/// `ids` must be readable for `len` ids, `frequencies` must be null or readable
/// for `len` values, and `out` must point at a writable [`KuraBuffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_postings_encode(
    ids: *const u32,
    frequencies: *const u32,
    len: usize,
    out: *mut KuraBuffer,
) -> i32 {
    if out.is_null() || (ids.is_null() && len > 0) {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: out was checked for null above. Writing the empty buffer first
        // means every path out leaves the caller with something freeable.
        unsafe { out.write(KuraBuffer::empty()) };

        // SAFETY: ids is non null whenever len is above zero, and the contract
        // says it is readable for that many ids.
        let ids = unsafe { as_slice(ids, len) };
        let freqs = if frequencies.is_null() {
            None
        } else {
            // SAFETY: frequencies is non null here, and the contract says it is
            // readable for the same count as ids.
            Some(unsafe { as_slice(frequencies, len) })
        };

        let mut writer = Writer::new();
        for (i, id) in ids.iter().enumerate() {
            let frequency = freqs.map_or(1, |values| values[i]);
            if let Err(err) = writer.push(*id, frequency) {
                return status_of(&err);
            }
        }

        // SAFETY: out was checked for null above.
        unsafe { out.write(KuraBuffer::from_vec(writer.finish())) };
        KURA_OK
    })
}

/// Writes how many document ids an encoded list holds into `out`.
///
/// # Safety
///
/// `data` must be readable for `len` bytes and `out` must point at a writable
/// `size_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_postings_len(data: *const u8, len: usize, out: *mut usize) -> i32 {
    if out.is_null() || (data.is_null() && len > 0) {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: data is non null whenever len is above zero, and the contract
        // says it is readable for that many bytes.
        let bytes = unsafe { as_slice(data, len) };
        match Reader::new(bytes) {
            Ok(reader) => {
                // SAFETY: out was checked for null above.
                unsafe { out.write(reader.len() as usize) };
                KURA_OK
            }
            Err(err) => status_of(&err),
        }
    })
}

/// Decodes an encoded list into `out`.
///
/// `out_len` always receives the number of ids in the list, so the usual call
/// pattern is [`kura_postings_len`] followed by one allocation and one decode.
///
/// # Safety
///
/// `data` must be readable for `len` bytes, `out` must be writable for `cap`
/// ids, and `out_len` must point at a writable `size_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_postings_decode(
    data: *const u8,
    len: usize,
    out: *mut u32,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() || (data.is_null() && len > 0) {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: out_len was checked for null above.
        unsafe { out_len.write(0) };

        // SAFETY: data is non null whenever len is above zero, and the contract
        // says it is readable for that many bytes.
        let bytes = unsafe { as_slice(data, len) };
        let reader = match Reader::new(bytes) {
            Ok(reader) => reader,
            Err(err) => return status_of(&err),
        };
        let ids = match reader.to_vec() {
            Ok(ids) => ids,
            Err(err) => return status_of(&err),
        };

        // SAFETY: out_len was checked for null above.
        unsafe { out_len.write(ids.len()) };
        if ids.len() > cap {
            return KURA_ERR_BUFFER_TOO_SMALL;
        }
        if ids.is_empty() {
            return KURA_OK;
        }
        if out.is_null() {
            return KURA_ERR_NULL;
        }
        // SAFETY: out is non null and the contract says it is writable for cap
        // ids, which the check above proved is at least ids.len().
        unsafe { core::ptr::copy_nonoverlapping(ids.as_ptr(), out, ids.len()) };
        KURA_OK
    })
}

/// Writes 1 or 0 into `out` depending on whether an encoded list holds `id`.
///
/// This decodes at most one block, so it stays cheap on a list with millions of
/// entries. It is the reason a host can ask a membership question without
/// pulling the list across the boundary.
///
/// # Safety
///
/// `data` must be readable for `len` bytes and `out` must point at a writable
/// `int`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_postings_contains(
    data: *const u8,
    len: usize,
    id: u32,
    out: *mut i32,
) -> i32 {
    if out.is_null() || (data.is_null() && len > 0) {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: out was checked for null above.
        unsafe { out.write(0) };

        // SAFETY: data is non null whenever len is above zero, and the contract
        // says it is readable for that many bytes.
        let bytes = unsafe { as_slice(data, len) };
        let reader = match Reader::new(bytes) {
            Ok(reader) => reader,
            Err(err) => return status_of(&err),
        };
        match reader.contains(id) {
            Ok(found) => {
                // SAFETY: out was checked for null above.
                unsafe { out.write(i32::from(found)) };
                KURA_OK
            }
            Err(err) => status_of(&err),
        }
    })
}

/// Writes the cosine similarity of two vectors into `out`.
///
/// # Safety
///
/// `a` must be readable for `a_len` floats, `b` for `b_len` floats, and `out`
/// must point at a writable `float`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_vector_cosine(
    a: *const f32,
    a_len: usize,
    b: *const f32,
    b_len: usize,
    out: *mut f32,
) -> i32 {
    if out.is_null() || (a.is_null() && a_len > 0) || (b.is_null() && b_len > 0) {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: out was checked for null above.
        unsafe { out.write(0.0) };

        // SAFETY: each pointer is non null whenever its length is above zero,
        // and the contract says it is readable for that many floats.
        let (left, right) = unsafe { (as_slice(a, a_len), as_slice(b, b_len)) };
        match vector::cosine(left, right) {
            Ok(score) => {
                // SAFETY: out was checked for null above.
                unsafe { out.write(score) };
                KURA_OK
            }
            Err(err) => status_of(&err),
        }
    })
}

/// Quantises a vector to one signed byte per dimension.
///
/// `out` receives one byte per input dimension and `out_scale` receives the
/// factor that maps 127 back to the original scale. Both are needed to
/// reconstruct the vector, so a caller that stores one without the other has
/// stored nothing.
///
/// # Safety
///
/// `input` must be readable for `len` floats, `out` must be writable for `len`
/// bytes, and `out_scale` must point at a writable `float`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_vector_quantise(
    input: *const f32,
    len: usize,
    out: *mut i8,
    out_scale: *mut f32,
) -> i32 {
    if out_scale.is_null() || (input.is_null() && len > 0) || (out.is_null() && len > 0) {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: input is non null whenever len is above zero, and the contract
        // says it is readable for that many floats.
        let input = unsafe { as_slice(input, len) };
        let quantised = vector::Quantised::from_f32(input);

        // SAFETY: out_scale was checked for null above.
        unsafe { out_scale.write(quantised.scale) };
        if quantised.values.is_empty() {
            return KURA_OK;
        }
        let values = quantised.values.as_ptr();
        let count = quantised.values.len();
        // SAFETY: out is non null whenever len is above zero, and the contract
        // says it is writable for that many bytes, which is exactly how many
        // values the quantiser produced.
        unsafe { core::ptr::copy_nonoverlapping(values, out, count) };
        KURA_OK
    })
}

/// Writes the dot product of two quantised vectors into `out`, back in the
/// original scale.
///
/// # Safety
///
/// `a` and `b` must each be readable for `len` bytes and `out` must point at a
/// writable `float`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kura_vector_dot_quantised(
    a: *const i8,
    a_scale: f32,
    b: *const i8,
    b_scale: f32,
    len: usize,
    out: *mut f32,
) -> i32 {
    if out.is_null() || ((a.is_null() || b.is_null()) && len > 0) {
        return KURA_ERR_NULL;
    }
    guard(|| {
        // SAFETY: out was checked for null above.
        unsafe { out.write(0.0) };

        // SAFETY: each pointer is non null whenever len is above zero, and the
        // contract says both are readable for that many bytes.
        let (left, right) = unsafe { (as_slice(a, len), as_slice(b, len)) };
        let left = vector::Quantised {
            scale: a_scale,
            values: left.to_vec(),
        };
        let right = vector::Quantised {
            scale: b_scale,
            values: right.to_vec(),
        };
        match left.dot(&right) {
            Ok(score) => {
                // SAFETY: out was checked for null above.
                unsafe { out.write(score) };
                KURA_OK
            }
            Err(err) => status_of(&err),
        }
    })
}

/// Builds a slice from a pointer and a length, treating a length of zero as an
/// empty slice whatever the pointer is.
///
/// # Safety
///
/// `data` must be readable for `len` elements unless `len` is zero.
unsafe fn as_slice<'a, T>(data: *const T, len: usize) -> &'a [T] {
    if len == 0 {
        return &[];
    }
    // SAFETY: the contract above says data is readable for len elements, which
    // is the whole of the caller's obligation.
    unsafe { slice::from_raw_parts(data, len) }
}

/// Maps an engine error onto a status code.
fn status_of(err: &Error) -> i32 {
    match err {
        Error::Truncated { .. } => KURA_ERR_TRUNCATED,
        Error::Overflow => KURA_ERR_OVERFLOW,
        Error::BadMagic => KURA_ERR_BAD_MAGIC,
        Error::UnsupportedVersion { .. } => KURA_ERR_UNSUPPORTED_VERSION,
        Error::ChecksumMismatch { .. } => KURA_ERR_CHECKSUM,
        Error::DimensionMismatch { .. } => KURA_ERR_DIMENSION_MISMATCH,
        Error::NotSorted { .. } => KURA_ERR_NOT_SORTED,
        // The error type is non exhaustive on purpose, so a new variant has to
        // be reportable before it is mapped.
        _ => KURA_ERR_PANIC,
    }
}

/// Runs the body of an entry point and turns a panic into a status code.
///
/// The closure is asserted to be unwind safe because nothing observes engine
/// state after a panic: the call reports failure, the caller is told the result
/// is not usable, and any half built allocation is dropped on the way out.
fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(KURA_ERR_PANIC)
}

/// The same guard for the constructors, which report failure with null.
fn guard_ptr<T, F: FnOnce() -> *mut T>(f: F) -> *mut T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(core::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_abi_version_and_crate_version_are_readable() {
        assert_eq!(kura_abi_version(), KURA_ABI_VERSION);
        // SAFETY: the pointer is a static string this crate owns.
        let version = unsafe { CStr::from_ptr(kura_version()) };
        assert_eq!(version.to_str().expect("utf8"), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn every_status_has_a_message() {
        for status in [
            KURA_OK,
            KURA_ERR_NULL,
            KURA_ERR_TRUNCATED,
            KURA_ERR_OVERFLOW,
            KURA_ERR_BAD_MAGIC,
            KURA_ERR_UNSUPPORTED_VERSION,
            KURA_ERR_CHECKSUM,
            KURA_ERR_DIMENSION_MISMATCH,
            KURA_ERR_NOT_SORTED,
            KURA_ERR_BUFFER_TOO_SMALL,
            KURA_ERR_PANIC,
            9999,
        ] {
            // SAFETY: the pointer is a static string this crate owns.
            let message = unsafe { CStr::from_ptr(kura_status_message(status)) };
            assert!(!message.to_bytes().is_empty(), "status {status}");
        }
    }

    #[test]
    fn frequencies_cross_the_boundary_when_they_are_given() {
        // Null means once in each, and a real array means what it says. Both go
        // through the same entry point, so the only way to know the pointer is
        // being read is to encode the same ids twice and look.
        let ids: Vec<u32> = (0..500u32).map(|i| i * 3).collect();
        let freqs: Vec<u32> = (0..500u32).map(|i| (i % 9) + 1).collect();

        let mut plain = KuraBuffer::empty();
        let mut scored = KuraBuffer::empty();
        // SAFETY: the pointers are to live local values.
        unsafe {
            assert_eq!(
                kura_postings_encode(ids.as_ptr(), core::ptr::null(), ids.len(), &raw mut plain),
                KURA_OK
            );
            assert_eq!(
                kura_postings_encode(ids.as_ptr(), freqs.as_ptr(), ids.len(), &raw mut scored),
                KURA_OK
            );
        }

        // SAFETY: both buffers came from a successful encode.
        let plain_bytes = unsafe { slice::from_raw_parts(plain.data, plain.len) };
        // SAFETY: as above.
        let scored_bytes = unsafe { slice::from_raw_parts(scored.data, scored.len) };

        let want: Vec<(u32, u32)> = ids.iter().copied().zip(freqs.iter().copied()).collect();
        let ones: Vec<(u32, u32)> = ids.iter().map(|id| (*id, 1)).collect();
        assert_eq!(
            Reader::new(scored_bytes)
                .expect("header")
                .to_postings()
                .expect("decode"),
            want
        );
        assert_eq!(
            Reader::new(plain_bytes)
                .expect("header")
                .to_postings()
                .expect("decode"),
            ones
        );

        // SAFETY: both buffers came from the engine and are freed once.
        unsafe {
            kura_buffer_free(plain);
            kura_buffer_free(scored);
        }
    }

    #[test]
    fn postings_round_trip_through_the_boundary() {
        let ids: Vec<u32> = (0..1_000u32).map(|i| i * 7).collect();

        let mut buffer = KuraBuffer::empty();
        // SAFETY: the pointers are to live local values.
        let status = unsafe {
            kura_postings_encode(ids.as_ptr(), core::ptr::null(), ids.len(), &raw mut buffer)
        };
        assert_eq!(status, KURA_OK);
        assert!(!buffer.data.is_null());

        let mut count = 0usize;
        // SAFETY: the buffer came from a successful encode.
        let status = unsafe { kura_postings_len(buffer.data, buffer.len, &raw mut count) };
        assert_eq!(status, KURA_OK);
        assert_eq!(count, ids.len());

        let mut decoded = vec![0u32; count];
        let mut written = 0usize;
        // SAFETY: decoded holds count ids, which is what the length call said.
        let status = unsafe {
            kura_postings_decode(
                buffer.data,
                buffer.len,
                decoded.as_mut_ptr(),
                decoded.len(),
                &raw mut written,
            )
        };
        assert_eq!(status, KURA_OK);
        assert_eq!(written, ids.len());
        assert_eq!(decoded, ids);

        let mut found = 0i32;
        // SAFETY: the buffer came from a successful encode.
        let status = unsafe { kura_postings_contains(buffer.data, buffer.len, 7, &raw mut found) };
        assert_eq!(status, KURA_OK);
        assert_eq!(found, 1);

        // SAFETY: the buffer came from a successful encode.
        let status = unsafe { kura_postings_contains(buffer.data, buffer.len, 8, &raw mut found) };
        assert_eq!(status, KURA_OK);
        assert_eq!(found, 0);

        // SAFETY: the buffer is the one the engine returned, freed once.
        unsafe { kura_buffer_free(buffer) };
    }

    #[test]
    fn a_short_output_buffer_reports_the_size_it_needs() {
        let ids: Vec<u32> = (0..300u32).collect();
        let mut buffer = KuraBuffer::empty();
        // SAFETY: the pointers are to live local values.
        let status = unsafe {
            kura_postings_encode(ids.as_ptr(), core::ptr::null(), ids.len(), &raw mut buffer)
        };
        assert_eq!(status, KURA_OK);

        let mut written = 0usize;
        // SAFETY: a capacity of zero is allowed with a null output pointer.
        let status = unsafe {
            kura_postings_decode(
                buffer.data,
                buffer.len,
                core::ptr::null_mut(),
                0,
                &raw mut written,
            )
        };
        assert_eq!(status, KURA_ERR_BUFFER_TOO_SMALL);
        assert_eq!(written, ids.len());

        // SAFETY: the buffer is the one the engine returned, freed once.
        unsafe { kura_buffer_free(buffer) };
    }

    #[test]
    fn out_of_order_ids_are_reported_not_silently_sorted() {
        let ids = [5u32, 4];
        let mut buffer = KuraBuffer::empty();
        // SAFETY: the pointers are to live local values.
        let status = unsafe {
            kura_postings_encode(ids.as_ptr(), core::ptr::null(), ids.len(), &raw mut buffer)
        };
        assert_eq!(status, KURA_ERR_NOT_SORTED);
        assert!(buffer.data.is_null());

        // SAFETY: freeing an empty buffer is defined to be a no op.
        unsafe { kura_buffer_free(buffer) };
    }

    #[test]
    fn garbage_input_is_an_error_not_a_crash() {
        let garbage = [0xffu8; 32];
        let mut count = 0usize;
        // SAFETY: the pointers are to live local values.
        let status = unsafe { kura_postings_len(garbage.as_ptr(), garbage.len(), &raw mut count) };
        assert_ne!(status, KURA_OK);
    }

    #[test]
    fn null_pointers_are_refused() {
        let mut count = 0usize;
        // SAFETY: passing null is the case under test.
        unsafe {
            assert_eq!(
                kura_postings_encode(
                    core::ptr::null(),
                    core::ptr::null(),
                    3,
                    core::ptr::null_mut()
                ),
                KURA_ERR_NULL
            );
            assert_eq!(
                kura_postings_len(core::ptr::null(), 3, &raw mut count),
                KURA_ERR_NULL
            );
            assert_eq!(kura_bitmap_insert(core::ptr::null_mut(), 1), KURA_ERR_NULL);
            assert_eq!(
                kura_bitmap_contains(core::ptr::null(), 1, core::ptr::null_mut()),
                KURA_ERR_NULL
            );
        }
    }

    #[test]
    fn bitmaps_intersect_across_the_boundary() {
        let left = kura_bitmap_new();
        let right = kura_bitmap_new();
        assert!(!left.is_null() && !right.is_null());

        // SAFETY: both handles came from kura_bitmap_new and are live.
        unsafe {
            for id in [1u32, 2, 3, 4, 5] {
                assert_eq!(kura_bitmap_insert(left, id), KURA_OK);
            }
            for id in [4u32, 5, 6] {
                assert_eq!(kura_bitmap_insert(right, id), KURA_OK);
            }
            assert_eq!(kura_bitmap_intersect(left, right), KURA_OK);

            let mut len = 0usize;
            assert_eq!(kura_bitmap_len(left, &raw mut len), KURA_OK);
            assert_eq!(len, 2);

            let mut ids = vec![0u32; len];
            let mut written = 0usize;
            assert_eq!(
                kura_bitmap_to_array(left, ids.as_mut_ptr(), ids.len(), &raw mut written),
                KURA_OK
            );
            assert_eq!(written, 2);
            assert_eq!(ids, vec![4, 5]);

            let mut found = 0i32;
            assert_eq!(kura_bitmap_contains(left, 1, &raw mut found), KURA_OK);
            assert_eq!(found, 0);

            assert_eq!(kura_bitmap_remove(left, 4), KURA_OK);
            assert_eq!(kura_bitmap_len(left, &raw mut len), KURA_OK);
            assert_eq!(len, 1);

            kura_bitmap_free(left);
            kura_bitmap_free(right);
        }
    }

    #[test]
    fn intersecting_a_bitmap_with_itself_is_allowed() {
        let bitmap = kura_bitmap_new();
        // SAFETY: the handle came from kura_bitmap_new and is live.
        unsafe {
            assert_eq!(kura_bitmap_insert(bitmap, 3), KURA_OK);
            assert_eq!(kura_bitmap_intersect(bitmap, bitmap), KURA_OK);
            assert_eq!(kura_bitmap_union(bitmap, bitmap), KURA_OK);

            let mut len = 0usize;
            assert_eq!(kura_bitmap_len(bitmap, &raw mut len), KURA_OK);
            assert_eq!(len, 1);
            kura_bitmap_free(bitmap);
        }
    }

    #[test]
    fn freeing_null_is_a_no_op() {
        // SAFETY: both functions define null as a no op.
        unsafe {
            kura_bitmap_free(core::ptr::null_mut());
            kura_buffer_free(KuraBuffer::empty());
        }
    }

    #[test]
    fn vectors_score_and_report_a_length_mismatch() {
        let a = [1.0f32, 2.0, 3.0];
        let b = [1.0f32, 2.0, 3.0];
        let mut score = 0.0f32;
        // SAFETY: the pointers are to live local values.
        let status =
            unsafe { kura_vector_cosine(a.as_ptr(), a.len(), b.as_ptr(), b.len(), &raw mut score) };
        assert_eq!(status, KURA_OK);
        assert!((score - 1.0).abs() < 1e-6);

        // SAFETY: the pointers are to live local values.
        let status =
            unsafe { kura_vector_cosine(a.as_ptr(), a.len(), b.as_ptr(), 2, &raw mut score) };
        assert_eq!(status, KURA_ERR_DIMENSION_MISMATCH);
    }

    #[test]
    fn quantised_vectors_score_across_the_boundary() {
        let a = [0.9f32, 0.1, -0.4, 0.2];
        let b = [0.85f32, 0.15, -0.35, 0.25];

        let mut qa = vec![0i8; a.len()];
        let mut qb = vec![0i8; b.len()];
        let mut scale_a = 0.0f32;
        let mut scale_b = 0.0f32;

        // SAFETY: every buffer holds one element per input dimension.
        unsafe {
            assert_eq!(
                kura_vector_quantise(a.as_ptr(), a.len(), qa.as_mut_ptr(), &raw mut scale_a),
                KURA_OK
            );
            assert_eq!(
                kura_vector_quantise(b.as_ptr(), b.len(), qb.as_mut_ptr(), &raw mut scale_b),
                KURA_OK
            );

            let mut score = 0.0f32;
            assert_eq!(
                kura_vector_dot_quantised(
                    qa.as_ptr(),
                    scale_a,
                    qb.as_ptr(),
                    scale_b,
                    qa.len(),
                    &raw mut score
                ),
                KURA_OK
            );
            assert!(score > 0.0, "similar vectors scored {score}");
        }
    }
}
