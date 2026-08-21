//! What a query paid in page faults, and how warm the index was before it ran.
//!
//! The counters in [`explain`](crate::explain) describe the walk. They say how
//! many postings were in the lists, how many were decoded and how many blocks
//! were stepped over, and none of that says anything about memory. A query that
//! decodes ten thousand postings out of a cold file and a query that decodes the
//! same ten thousand out of page cache do identical work by every number those
//! counters print, and differ by two orders of magnitude in time. That gap is
//! where a cold start regression hides.
//!
//! So this module asks the operating system two different questions and reports
//! both, because either one on its own is misleading.
//!
//! How warm was the index. Answered by walking the pages the index occupies and
//! asking which of them are resident, once, before the query starts. This is the
//! denominator for everything else: a query that faulted nothing because the
//! whole file was already in memory and a query that faulted nothing because it
//! touched almost none of the file look identical without it.
//!
//! What did the query fault. Answered by reading the fault counters before and
//! after and subtracting. Where the platform separates faults that needed a read
//! from disk from faults that only needed a page table entry, both are reported,
//! because those two differ by four orders of magnitude in cost and calling them
//! by one name would hide the whole point.
//!
//! # This is the one place in the crate with platform code
//!
//! Everything else here is arithmetic over byte slices, and that is worth
//! keeping. This lives in the engine anyway, rather than in the tool or in the
//! benchmark runner, because the alternative is two implementations of three
//! platforms in two repositories drifting apart until a number measured against
//! one of them stops describing the other. That is a bug we have already had
//! once and would rather not have twice.
//!
//! There are still no dependencies. Three function declarations and a couple of
//! structs per platform is a smaller thing to own than a crate.
//!
//! # What this cannot tell you
//!
//! Faults are counted against a thread or a process, never against a query. On
//! Linux the counter is per thread, so a query in one thread is not polluted by
//! a query in another. On the Apple systems, the BSDs and Windows it is per
//! process, so a reading taken while the process is doing something else
//! includes the something else. A single threaded tool answering one query is
//! exact everywhere. A benchmark running fourteen workers is not, on three
//! platforms out of four, and saying so is better than printing a number that
//! looks per query and is not.
//!
//! Residency does not mean the same thing on every system and the difference is
//! not written down anywhere useful. Measured here, a file the machine has
//! entirely in cache, freshly mapped into a new process, reads as one percent
//! resident on the Apple systems, because what comes back describes the mapping
//! and not the cache behind it. So read this number as how much of this mapping
//! is ready to be used without a fault, which is the question a query actually
//! has, and not as how much of the file the machine has somewhere in memory.
//! The `faulted from disk` count is the one that separates those two, and it is
//! why both are reported.
//!
//! Faults are counted and not measured. A size is the count times the page size,
//! and a single fault can hand over more than one page: Linux gives out
//! anonymous memory two megabytes at a time when transparent huge pages are on,
//! so a ninety six megabyte region is twenty four thousand pages and forty eight
//! faults. A file backed mapping, which is what an index is, takes ordinary page
//! sized faults on every system we build for, so for the case this exists to
//! measure the two agree. The counts are exact either way, which is why they are
//! what the type carries and the sizes are derived from them.

use crate::explain::Counters;

/// How much of the index was in memory, and what the query had to fetch.
///
/// Every field a platform cannot answer is `None` rather than zero. A zero here
/// reads as an answer and would be believed, and a counter that lies is worse
/// than a counter that is missing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Residency {
    /// How many times the query faulted, from storage or from cache.
    pub faults: Option<u64>,
    /// How many of those faults needed a read from storage.
    ///
    /// The expensive kind. A query where this is zero and `faults` is large paid
    /// for page table entries, which is microseconds. A query where the two are
    /// equal paid for storage, which is milliseconds.
    pub faults_from_disk: Option<u64>,
    /// How many bytes of the index were already resident when the query started.
    pub resident_before: Option<u64>,
    /// How many bytes the index is, so the rest have a denominator.
    pub total: u64,
    /// How big a page is here, which is what turns a count into a size.
    pub page: u64,
    /// Why something above is missing, when something is.
    pub note: Option<&'static str>,
}

impl Residency {
    /// How many bytes the query faulted in, at least.
    ///
    /// At least, because this is the fault count times the page size and a
    /// single fault can bring in more than one page. Linux does that routinely:
    /// a ninety six megabyte anonymous region is twenty four thousand pages and
    /// faults forty eight times, because transparent huge pages hand it over two
    /// megabytes at a time. A file backed mapping, which is what an index is,
    /// takes ordinary page sized faults on every system we build for, so for the
    /// case this exists to measure the floor and the answer are the same number.
    /// It is worth knowing which one is being read.
    #[must_use]
    pub fn faulted(&self) -> Option<u64> {
        self.faults.map(|faults| faults.saturating_mul(self.page))
    }

    /// How many of those bytes came from storage, at least.
    ///
    /// The same caveat as [`Residency::faulted`].
    #[must_use]
    pub fn faulted_from_disk(&self) -> Option<u64> {
        self.faults_from_disk
            .map(|faults| faults.saturating_mul(self.page))
    }
    /// What fraction of the index was resident before the query.
    ///
    /// One is a fully warm file, zero is a cold one, and `None` is either an
    /// empty index or a platform that will not say.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "a ratio of two byte counts, where the last few bits of a file \
                  sized integer do not change the answer to two decimal places"
    )]
    pub fn warm(&self) -> Option<f32> {
        if self.total == 0 {
            return None;
        }
        let resident = self.resident_before?;
        Some(resident.min(self.total) as f32 / self.total as f32)
    }
}

/// A reading taken before a query, waiting for the one taken after it.
///
/// Made by [`Probe::start`] and spent by [`Probe::finish`]. Nothing is measured
/// unless one of these exists, which is what keeps the cost at nothing for the
/// callers that never ask.
#[derive(Debug)]
pub struct Probe {
    /// The fault counters as they stood before the query, or why not.
    before: Result<Faults, &'static str>,
    /// The residency reading, taken once at the start.
    resident_before: Option<u64>,
    /// Why there is no residency reading, when there is not.
    why_not: Option<&'static str>,
    /// How big the index this probe was started on is.
    total: u64,
    /// How big a page is here, for turning fault counts into bytes.
    page: u64,
}

impl Probe {
    /// Takes the before reading.
    ///
    /// The slice is the whole index as it sits in memory. Handing over a
    /// subslice asks about the pages that subslice shares with its neighbours,
    /// which is a different question with a similar looking answer.
    ///
    /// This walks one byte per page of the index, so it is linear in the size of
    /// the file and not free. On a 700 MB index it is a scan of about 170
    /// kilobytes, which is tens of microseconds. It is charged to the caller who
    /// asked for the measurement and to nobody else.
    #[must_use]
    pub fn start(index: &[u8]) -> Self {
        let page = platform::page_size();
        let (resident_before, why_not) = if index.is_empty() {
            (Some(0), None)
        } else {
            match platform::resident_bytes(index, page) {
                Ok(bytes) => (Some(bytes), None),
                Err(why) => (None, Some(why)),
            }
        };
        // The fault counters are read last, after the scan above has allocated
        // and touched its own buffer. Reading them first would charge the query
        // for the faults this probe took to set itself up.
        Self {
            before: platform::faults(),
            resident_before,
            why_not,
            total: as_u64(index.len()),
            page: as_u64(page),
        }
    }

    /// Takes the after reading and returns the difference.
    #[must_use]
    pub fn finish(self) -> Residency {
        let after = platform::faults();
        let mut residency = Residency {
            faults: None,
            faults_from_disk: None,
            resident_before: self.resident_before,
            total: self.total,
            page: self.page,
            note: self.why_not,
        };

        match (self.before, after) {
            (Ok(before), Ok(after)) => {
                // Saturating, because these counters only go up and a
                // subtraction that went the other way means the platform handed
                // back something we do not understand. Zero is the truthful
                // answer to "how many more than before". A wrapped count of
                // eighteen quintillion is not.
                residency.faults = Some(after.faults.saturating_sub(before.faults));
                residency.faults_from_disk = match (before.from_disk, after.from_disk) {
                    (Some(before), Some(after)) => Some(after.saturating_sub(before)),
                    _ => None,
                };
            }
            (Err(why), _) | (_, Err(why)) => residency.note = residency.note.or(Some(why)),
        }
        residency
    }
}

/// Runs a query with a probe around it and hangs the reading on its counters.
///
/// The shape every caller wants, so that the before reading, the query and the
/// after reading cannot drift apart in a refactor. The closure returns what the
/// explained search calls return, which is an answer and a set of counters, or
/// an error.
///
/// A query that failed is not measured. There is no counters to hang the
/// reading on and no answer whose cost anybody wants.
///
/// # Errors
///
/// Returns whatever the closure returned, unchanged.
pub fn measured<T, E>(
    index: &[u8],
    body: impl FnOnce() -> Result<(T, Counters), E>,
) -> Result<(T, Counters), E> {
    let probe = Probe::start(index);
    let (answer, mut counters) = body()?;
    counters.residency = Some(probe.finish());
    Ok((answer, counters))
}

/// The most memory this process has had resident at once, in bytes.
///
/// A high water mark rather than a reading of this moment, which is what every
/// system here keeps and is also the number an operator is actually worried
/// about. Nothing lowers it, so two readings taken either side of a piece of
/// work say how much that piece of work added to the worst the process has ever
/// been, and a difference of zero means it stayed under a mark something earlier
/// had already set.
///
/// It is the process and not this crate. Anything else the program does is in
/// here too, which is exact for a tool that indexes and then exits and is not
/// exact for a server that indexes while it serves. Read it as the same number
/// `/usr/bin/time` prints, because on the systems that have both it is.
///
/// This is a different question from [`Held`](crate::index::Held), and both are
/// needed. `Held` is what the engine knows it is holding, this is what the
/// operating system says the process has, and the gap between them is the
/// allocator, the buffers a finished segment is built in and the pages of any
/// file that has just been written.
///
/// # Errors
///
/// Returns a message if the system will not answer, which today means the web,
/// where there is no process to ask about.
pub fn peak_resident() -> Result<u64, &'static str> {
    platform::peak_resident()
}

/// A length or a count as the width the report uses.
///
/// `usize` is never wider than `u64` on anything we build for, so the fallback
/// is unreachable rather than approximate, and it is here so that the conversion
/// is written once instead of at six call sites.
fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Fault counts as one platform reports them.
#[derive(Debug, Clone, Copy)]
struct Faults {
    /// Every fault of either kind, in pages.
    faults: u64,
    /// The faults that needed a read from storage, in pages, where known.
    from_disk: Option<u64>,
}

#[cfg(unix)]
mod platform {
    //! Unix, meaning Linux, the Apple systems and the BSDs.
    //!
    //! `getrusage` has the fault counts and `mincore` has residency, and both
    //! are old enough that every system we build for has them.
    //!
    //! The divergence worth naming is the first argument to `getrusage`. Linux
    //! has `RUSAGE_THREAD`, which makes the counts per thread and therefore
    //! genuinely per query when queries run on their own threads. Nothing else
    //! has it, so everything else asks about the whole process.

    use super::Faults;

    /// The whole calling process.
    #[cfg(not(target_os = "linux"))]
    const RUSAGE_WHO: i32 = 0;
    /// The calling thread, which only Linux has.
    #[cfg(target_os = "linux")]
    const RUSAGE_WHO: i32 = 1;

    /// The whole calling process, asked for by name.
    ///
    /// The high water mark is kept for the process and not for the thread, so
    /// this is the one to ask about it with even where the fault counts come
    /// from somewhere narrower.
    const RUSAGE_PROCESS: i32 = 0;

    /// `mincore` sets this bit when the page is resident.
    ///
    /// Linux defines only this bit. The Apple systems define several more and
    /// this is the first of them, so one mask serves both.
    const RESIDENT: u8 = 1;

    /// Where `ru_maxrss` sits among the longs, which is first.
    const MAXRSS: usize = 0;

    /// What `ru_maxrss` counts in.
    ///
    /// Bytes on the Apple systems and kilobytes on Linux and the BSDs. The two
    /// differ by a factor of a thousand and nothing in the value says which one
    /// it is, so this is the whole of the difference and it is written down here
    /// rather than found out later by somebody wondering why a laptop reported a
    /// hundred gigabytes.
    #[cfg(target_vendor = "apple")]
    const MAXRSS_UNIT: u64 = 1;
    /// What `ru_maxrss` counts in.
    #[cfg(not(target_vendor = "apple"))]
    const MAXRSS_UNIT: u64 = 1024;

    /// Where `ru_minflt` sits among the longs.
    const MINOR: usize = 4;
    /// Where `ru_majflt` sits among the longs.
    const MAJOR: usize = 5;

    /// `struct rusage`, as much of it as is read and no more.
    ///
    /// Two time values first, then a run of longs, of which the fifth and sixth
    /// are the fault counts. That is true on Linux, on the Apple systems and on
    /// the BSDs, which differ in how long the run is and not in what starts it.
    /// The array is longer than the real structure on all of them, and the
    /// kernel writes only as much as its own definition holds, so the extra room
    /// is never touched and never read.
    #[repr(C)]
    struct Rusage {
        /// User time, as a pair of words, whatever this system's `timeval` is.
        user: [i64; 2],
        /// System time, the same.
        system: [i64; 2],
        /// `ru_maxrss` onwards, of which two are read.
        longs: [i64; 16],
    }

    impl Rusage {
        /// Somewhere for the kernel to write.
        const fn zeroed() -> Self {
            Self {
                user: [0; 2],
                system: [0; 2],
                longs: [0; 16],
            }
        }
    }

    // SAFETY: these match the declarations in the system headers. `getrusage`
    // writes a `struct rusage` through the pointer, `mincore` writes one byte
    // per page of the range, and `getpagesize` takes nothing and cannot fail.
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
        fn mincore(addr: *mut core::ffi::c_void, length: usize, vec: *mut u8) -> i32;
        fn getpagesize() -> i32;
    }

    /// How many bytes a page is on this machine.
    pub fn page_size() -> usize {
        // SAFETY: no arguments, no pointers, and no failure mode.
        let size = unsafe { getpagesize() };
        // Anything that is not a positive power of two is a system nobody here
        // understands, and four kilobytes is the answer that leaves the
        // arithmetic below harmless rather than wrong.
        usize::try_from(size)
            .ok()
            .filter(|size| size.is_power_of_two())
            .unwrap_or(4096)
    }

    /// Reads the fault counters.
    pub fn faults() -> Result<Faults, &'static str> {
        let mut usage = Rusage::zeroed();
        // SAFETY: `usage` is live, correctly aligned, and at least as large as
        // this system's `struct rusage`. `RUSAGE_WHO` is a value this system
        // defines.
        let rc = unsafe { getrusage(RUSAGE_WHO, &raw mut usage) };
        if rc != 0 {
            return Err("getrusage refused, so this process cannot see its own fault counts");
        }
        Ok(Faults {
            faults: usage.longs[MINOR].max(0).unsigned_abs()
                + usage.longs[MAJOR].max(0).unsigned_abs(),
            from_disk: Some(usage.longs[MAJOR].max(0).unsigned_abs()),
        })
    }

    /// Reads the high water mark.
    pub fn peak_resident() -> Result<u64, &'static str> {
        let mut usage = Rusage::zeroed();
        // SAFETY: `usage` is live, correctly aligned, and at least as large as
        // this system's `struct rusage`.
        let rc = unsafe { getrusage(RUSAGE_PROCESS, &raw mut usage) };
        if rc != 0 {
            return Err("getrusage refused, so this process cannot see its own high water mark");
        }
        Ok(usage.longs[MAXRSS]
            .max(0)
            .unsigned_abs()
            .saturating_mul(MAXRSS_UNIT))
    }

    /// Counts the bytes of `index` that are resident.
    pub fn resident_bytes(index: &[u8], page: usize) -> Result<u64, &'static str> {
        // `mincore` wants a page boundary. A mapped file starts on one, so this
        // is a no op for the case that matters, and for anything else it widens
        // the question to the pages the slice shares with its neighbours, which
        // is the honest answer to a question about pages.
        let start = index.as_ptr() as usize;
        let aligned = start & !(page - 1);
        let length = index.len() + (start - aligned);
        let pages = length.div_ceil(page);

        let mut resident = vec![0u8; pages];
        // SAFETY: `aligned` is a page boundary at or below a live allocation,
        // `length` covers that allocation and nothing past the page it ends on,
        // and `resident` holds one byte per page of `length`, which is what the
        // call requires.
        let rc = unsafe {
            mincore(
                aligned as *mut core::ffi::c_void,
                length,
                resident.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return Err("mincore refused the range, so these pages are not a mapping");
        }

        let count = resident.iter().filter(|page| *page & RESIDENT != 0).count();
        Ok(super::as_u64(count.saturating_mul(page)))
    }
}

#[cfg(windows)]
mod platform {
    //! Windows.
    //!
    //! `GetProcessMemoryInfo` has a fault count and `QueryWorkingSetEx` has
    //! residency. Both are called through their `kernel32` forwarders so that
    //! nothing has to be added to the link line.
    //!
    //! Two things are worse here than on unix and both are reported rather than
    //! papered over. The fault count is for the whole process, with no per
    //! thread equivalent. And it is one number, with no split between faults
    //! that went to storage and faults that did not, so `faulted_from_disk` is
    //! `None` on this platform and stays `None`.

    use super::Faults;

    /// `PSAPI_WORKING_SET_EX_BLOCK` sets this bit when the page is resident.
    const VALID: usize = 1;

    /// `SYSTEM_INFO`, in full because the call writes all of it.
    #[repr(C)]
    struct SystemInfo {
        oem_id: u32,
        /// What this is called for.
        page_size: u32,
        minimum_application_address: *mut core::ffi::c_void,
        maximum_application_address: *mut core::ffi::c_void,
        active_processor_mask: usize,
        number_of_processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        processor_level: u16,
        processor_revision: u16,
    }

    /// `PROCESS_MEMORY_COUNTERS`, in full because `cb` has to match its size.
    #[repr(C)]
    struct ProcessMemoryCounters {
        /// The size of this structure, which the call checks.
        cb: u32,
        /// One of the two things this is called for.
        page_fault_count: u32,
        /// The other one.
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    /// `PSAPI_WORKING_SET_EX_INFORMATION`, one per page asked about.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct WorkingSetExInformation {
        /// The page being asked about, which the caller fills in.
        virtual_address: *mut core::ffi::c_void,
        /// What comes back, of which one bit is read.
        attributes: usize,
    }

    // SAFETY: these match the declarations in the Windows headers. The two `K32`
    // names are the `kernel32` forwarders for the `psapi` entry points, present
    // since Windows 7, and using them is what keeps this from needing another
    // library on the link line.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetSystemInfo"]
        fn get_system_info(info: *mut SystemInfo);
        #[link_name = "GetCurrentProcess"]
        fn current_process() -> *mut core::ffi::c_void;
        #[link_name = "K32GetProcessMemoryInfo"]
        fn process_memory_info(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        #[link_name = "K32QueryWorkingSetEx"]
        fn query_working_set_ex(
            process: *mut core::ffi::c_void,
            buffer: *mut core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    /// How many bytes a page is on this machine.
    pub fn page_size() -> usize {
        let mut info = SystemInfo {
            oem_id: 0,
            page_size: 0,
            minimum_application_address: core::ptr::null_mut(),
            maximum_application_address: core::ptr::null_mut(),
            active_processor_mask: 0,
            number_of_processors: 0,
            processor_type: 0,
            allocation_granularity: 0,
            processor_level: 0,
            processor_revision: 0,
        };
        // SAFETY: `info` is a live, correctly aligned `SYSTEM_INFO`, which is
        // exactly what the call writes.
        unsafe { get_system_info(&raw mut info) };
        usize::try_from(info.page_size)
            .ok()
            .filter(|size| size.is_power_of_two())
            .unwrap_or(4096)
    }

    /// Reads the process counters, which two of these functions want.
    fn counters(refused: &'static str) -> Result<ProcessMemoryCounters, &'static str> {
        let mut counters = ProcessMemoryCounters {
            cb: 0,
            page_fault_count: 0,
            peak_working_set_size: 0,
            working_set_size: 0,
            quota_peak_paged_pool_usage: 0,
            quota_paged_pool_usage: 0,
            quota_peak_non_paged_pool_usage: 0,
            quota_non_paged_pool_usage: 0,
            pagefile_usage: 0,
            peak_pagefile_usage: 0,
        };
        let size = u32::try_from(size_of::<ProcessMemoryCounters>())
            .map_err(|_| "the memory counters structure is larger than the call can describe")?;
        counters.cb = size;
        // SAFETY: `counters` is live, correctly aligned and exactly `size` bytes
        // long, and the handle is the current process pseudo handle, which is a
        // constant and needs no closing.
        let ok = unsafe { process_memory_info(current_process(), &raw mut counters, size) };
        if ok == 0 {
            return Err(refused);
        }
        Ok(counters)
    }

    /// Reads the high water mark.
    pub fn peak_resident() -> Result<u64, &'static str> {
        let counters = counters(
            "GetProcessMemoryInfo refused, so this process cannot see its high water mark",
        )?;
        Ok(super::as_u64(counters.peak_working_set_size))
    }

    /// Reads the fault counter.
    pub fn faults() -> Result<Faults, &'static str> {
        let counters =
            counters("GetProcessMemoryInfo refused, so this process cannot see its fault count")?;
        Ok(Faults {
            faults: u64::from(counters.page_fault_count),
            // Windows counts faults but does not say which of them went to
            // storage. Reporting nothing is the truth. Reporting zero would read
            // as a perfectly warm file every single time.
            from_disk: None,
        })
    }

    /// Counts the bytes of `index` that are resident.
    pub fn resident_bytes(index: &[u8], page: usize) -> Result<u64, &'static str> {
        let start = index.as_ptr() as usize;
        let aligned = start & !(page - 1);
        let length = index.len() + (start - aligned);
        let pages = length.div_ceil(page);

        // One entry per page, each naming the page it asks about. This is
        // sixteen bytes a page rather than the one byte unix wants, so a 700 MB
        // index costs a 2.7 MB buffer here. It is allocated by the caller who
        // asked for the measurement and freed when this returns.
        let mut query: Vec<WorkingSetExInformation> = (0..pages)
            .map(|at| WorkingSetExInformation {
                virtual_address: (aligned + at * page) as *mut core::ffi::c_void,
                attributes: 0,
            })
            .collect();
        let bytes = u32::try_from(size_of::<WorkingSetExInformation>().saturating_mul(pages))
            .map_err(|_| "the index has more pages than one call can ask about")?;

        // SAFETY: `query` is a live array of `pages` entries, `bytes` is exactly
        // its size, every entry names an address the caller owns, and the handle
        // is the current process pseudo handle.
        let ok =
            unsafe { query_working_set_ex(current_process(), query.as_mut_ptr().cast(), bytes) };
        if ok == 0 {
            return Err("QueryWorkingSetEx refused the range, so these pages are not a mapping");
        }

        let count = query
            .iter()
            .filter(|entry| entry.attributes & VALID != 0)
            .count();
        Ok(super::as_u64(count.saturating_mul(page)))
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    //! Everywhere else, which today means the web.
    //!
    //! There are no pages to be resident in and no faults to count, and this
    //! says so rather than returning zeroes that would read as a perfectly warm
    //! index that never faulted.

    use super::Faults;

    /// What the arithmetic assumes when there is nothing to ask.
    pub fn page_size() -> usize {
        4096
    }

    /// Says there is nothing to read.
    pub fn faults() -> Result<Faults, &'static str> {
        Err("this platform does not account for page faults")
    }

    /// Says there is nothing to ask.
    pub fn peak_resident() -> Result<u64, &'static str> {
        Err("this platform has no process whose memory can be counted")
    }

    /// Says there is nothing to ask.
    pub fn resident_bytes(_index: &[u8], _page: usize) -> Result<u64, &'static str> {
        Err("this platform has no pages for an index to be resident in")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Large enough that the faults from it dwarf whatever else the test binary
    /// is doing on its other threads, and small enough to be polite.
    ///
    /// Deliberately not the same size as [`WARM`]. An allocator hands a freed
    /// block of one size straight back to the next request for that size, and a
    /// recycled block has already been faulted in, so two tests asking for the
    /// same amount would leave this one measuring nothing. That is not
    /// hypothetical: it is what the first version of this test did, on a machine
    /// with sixteen kilobyte pages.
    const REGION: usize = 96 << 20;

    /// Enough to be worth counting pages of, and nothing like [`REGION`].
    const WARM: usize = 4 << 20;

    #[test]
    fn a_page_is_a_positive_power_of_two() {
        let page = platform::page_size();
        assert!(page >= 4096, "a page of {page} bytes");
        assert!(page.is_power_of_two(), "a page of {page} bytes");
    }

    #[test]
    fn an_empty_index_is_answered_rather_than_refused() {
        let residency = Probe::start(&[]).finish();
        assert_eq!(residency.total, 0);
        assert_eq!(residency.resident_before, Some(0));
        assert_eq!(residency.warm(), None);
    }

    #[test]
    fn touching_fresh_pages_faults_and_touching_them_again_does_not() {
        // The measurement this module exists for, on memory whose history the
        // test controls. An allocation this size comes from the system rather
        // than from a free list, and the system hands back pages that are
        // promised and not yet delivered, so the first pass over it collects
        // them and the second pass finds them already there.
        let mut region = vec![0u8; REGION];
        let page = platform::page_size();

        // Started on an empty slice on purpose. The residency scan allocates,
        // and a scan of this region would fault pages of its own buffer into
        // the window being measured.
        //
        // The region is handed to `black_box` after each pass because nothing
        // in the test reads what was written, and an optimiser that can see
        // that is entitled to drop the first pass entirely: the second one
        // overwrites every byte it touched. Which is what an optimising build
        // did, leaving the first pass with no faults to its name and the test
        // failing on a claim that was true about the source and not about the
        // program. `black_box` makes the address opaque, so the stores before
        // it have to happen.
        let first = Probe::start(&[]);
        for at in (0..REGION).step_by(page) {
            region[at] = 1;
        }
        core::hint::black_box(&region);
        let first = first.finish();

        let second = Probe::start(&[]);
        for at in (0..REGION).step_by(page) {
            region[at] = 2;
        }
        core::hint::black_box(&region);
        let second = second.finish();

        let Some(first) = first.faults else {
            // A platform that says it cannot answer is allowed to say so, and
            // this test is about the ones that can.
            assert!(first.note.is_some());
            return;
        };
        let second = second.faults.expect("the same platform answered once");

        // Counts rather than sizes, and a floor of forty rather than one page
        // per page of the region. Linux hands anonymous memory over two megabytes
        // at a time, so ninety six megabytes is forty eight faults there and
        // twenty four thousand on a system without huge pages. Both are the first
        // pass collecting the region, which is what is under test.
        assert!(
            first >= 40,
            "the first pass over {REGION} bytes faulted {first} times"
        );
        // Not zero, because the other test threads are faulting too and the
        // counter is process wide on three platforms out of four. A tenth is far
        // below anything a real second pass could cost and far above the noise.
        assert!(
            second < first / 10,
            "the second pass faulted {second} times against {first} for the first"
        );
    }

    #[test]
    fn a_region_that_was_just_written_reads_as_warm() {
        // Written before it is asked about, so every page of it is resident and
        // the answer is known in advance. What is under test is the arithmetic:
        // pages counted, multiplied by the page size, against the length.
        let region = vec![7u8; WARM];
        let residency = Probe::start(&region).finish();

        assert_eq!(residency.total, as_u64(WARM));
        let Some(warm) = residency.warm() else {
            assert!(residency.note.is_some());
            return;
        };
        assert!(warm > 0.9, "a region just written read as {warm} warm");
    }

    #[test]
    fn a_missing_answer_comes_with_a_reason() {
        // Whichever way this build went, the pairing has to hold: a field is
        // either answered or explained. A `None` with no note beside it is the
        // failure this guards against, because it cannot be told apart from
        // nobody having asked.
        let residency = Probe::start(&[1u8; 64]).finish();
        if residency.faults.is_none() || residency.resident_before.is_none() {
            assert!(residency.note.is_some(), "{residency:?}");
        }
    }

    #[test]
    fn faults_from_disk_are_never_more_than_faults() {
        let residency = Probe::start(&[]).finish();

        if let (Some(faults), Some(from_disk)) = (residency.faults, residency.faults_from_disk) {
            assert!(from_disk <= faults, "{from_disk} of {faults} from disk");
        }
    }

    #[test]
    fn a_size_is_the_count_times_the_page() {
        let residency = Residency {
            faults: Some(3),
            faults_from_disk: Some(1),
            resident_before: Some(0),
            total: 1 << 20,
            page: 4096,
            note: None,
        };
        assert_eq!(residency.faulted(), Some(12288));
        assert_eq!(residency.faulted_from_disk(), Some(4096));

        let unknown = Residency::default();
        assert_eq!(unknown.faulted(), None);
        assert_eq!(unknown.faulted_from_disk(), None);
    }

    #[test]
    fn measured_hangs_the_reading_on_the_counters() {
        let region = vec![0u8; WARM];
        let (answer, counters) =
            measured(&region, || Ok::<_, ()>((41 + 1, Counters::default()))).expect("succeeds");

        assert_eq!(answer, 42);
        let residency = counters.residency.expect("the probe ran");
        assert_eq!(residency.total, as_u64(region.len()));
    }

    #[test]
    fn a_query_that_failed_is_not_measured() {
        let region = vec![0u8; WARM];
        let failed: Result<((), Counters), &str> = measured(&region, || Err("no such term"));
        assert_eq!(failed.err(), Some("no such term"));
    }

    #[test]
    fn a_high_water_mark_never_falls() {
        // Which is the whole property two readings either side of a piece of
        // work rely on. If it could fall, a difference of zero would mean
        // nothing rather than meaning the work stayed under an earlier mark.
        let Ok(before) = peak_resident() else {
            return;
        };
        let mut region = vec![0u8; 64 << 20];
        // Written to rather than only allocated, because a page nothing has
        // touched is a page the system has not had to find memory for.
        for at in (0..region.len()).step_by(4096) {
            region[at] = 1;
        }
        let after = peak_resident().expect("the same call answered a moment ago");
        assert!(after >= before, "{after} is below {before}");
        drop(region);
        let later = peak_resident().expect("and again");
        assert!(later >= after, "{later} fell back to below {after}");
    }

    #[test]
    fn a_reading_is_in_bytes_and_not_in_whatever_the_system_felt_like() {
        // The unit is the one thing about this that differs between systems and
        // the one thing a caller cannot check for itself. A test binary that
        // has already allocated is worth more than a megabyte and cannot be
        // worth a terabyte, and those bounds are three orders of magnitude
        // apart, which is the mistake being guarded against.
        let Ok(peak) = peak_resident() else {
            return;
        };
        assert!(peak > 1 << 20, "{peak} bytes is too little to be a process");
        assert!(peak < 1 << 40, "{peak} bytes is too much to be this one");
    }
}
