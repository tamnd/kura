//! Which call a commit makes, and what that call does not promise.
//!
//! A store that says a returned commit survives a power cut is making a claim
//! about a platform call, and the claim is only as good as the call. The
//! trouble is that the obvious call does not mean the same thing everywhere,
//! and on one of the platforms this engine runs on it means considerably less
//! than most people writing it believe.
//!
//! On macOS an `fsync` returns once the write has been handed to the drive. The
//! drive is entitled to hold it in a volatile cache and acknowledge it anyway,
//! and a power cut at that moment loses it. `F_FULLFSYNC` is the call that asks
//! the drive to empty that cache, and it is the one Apple says to use when the
//! write has to survive. On Linux and on Windows the ordinary call already asks
//! for the flush, so there is nothing stronger to ask for.
//!
//! That asymmetry is why this is a named thing rather than a line inside the
//! commit. A commit latency measured with the weaker call on macOS and compared
//! against one measured with the stronger call on Linux is not a comparison, and
//! the only way to keep it from happening quietly is to make the caller say
//! which promise it wanted and to print the call's real name beside the number.
//!
//! # What it costs
//!
//! The `sync` example measures it, 500 syncs, each after a 4 KiB write to a
//! file already at its full length.
//!
//! An M4 laptop on APFS:
//!
//! | reach | call | median | p99 | max | syncs/s |
//! | --- | --- | --- | --- | --- | --- |
//! | [`Reach::Platter`] | `F_FULLFSYNC` | 3.86 ms | 4.90 ms | 11.94 ms | 259 |
//! | [`Reach::Device`] | `fsync` | 3.89 ms | 6.01 ms | 7.16 ms | 257 |
//! | [`Reach::Ordered`] | `F_BARRIERFSYNC` | 0.81 ms | 1.24 ms | 1.84 ms | 1,233 |
//!
//! A four core Linux box on ext4 over SATA, where all three are one call:
//!
//! | reach | call | median | p99 | max | syncs/s |
//! | --- | --- | --- | --- | --- | --- |
//! | any | `fdatasync` | 13.71 ms | 115.26 ms | 239.62 ms | 73 |
//!
//! The example was run on that box again later, and because the three reaches
//! are one call there it is three measurements of the same thing:
//!
//! | reach | call | median | p99 | max | syncs/s |
//! | --- | --- | --- | --- | --- | --- |
//! | [`Reach::Platter`] | `fdatasync` | 2.715 ms | 27.259 ms | 38.799 ms | 368 |
//! | [`Reach::Device`] | `fdatasync` | 4.593 ms | 93.927 ms | 274.030 ms | 218 |
//! | [`Reach::Ordered`] | `fdatasync` | 5.241 ms | 37.612 ms | 47.161 ms | 191 |
//!
//! Nothing separates those rows but the machine, and they are a factor of two
//! apart at the median and a factor of ten at the worst. That is the floor on
//! how finely any single sync measurement can be read, and it is why the
//! decision the module doc argues for is made on which call was asked for
//! rather than on which one measured faster on the day.
//!
//! The honest call on the laptop is free, inside the noise of the weaker one,
//! and that is what decides the default. Somewhere it will cost a great deal
//! more, and there the weaker reaches are here to be asked for deliberately.
//!
//! The other thing those tables say is that the worst sync is three times the
//! median on one machine and seventeen times the median on the other, so a
//! commit latency reported as a mean has hidden the only part of it anybody was
//! worried about.
//!
//! Both tables are of an idle machine, and the idle number is the flattering
//! one. The same laptop measured immediately after three indexing runs, with
//! the writes they left still going down, gave [`Reach::Platter`] a median of
//! 5.47 ms, a p99 of 452 ms and a worst of 1.97 s, against a p99 of 6.31 ms for
//! [`Reach::Device`] on the same run. Asking the drive to empty its cache means
//! waiting for whatever else is in it, so the call that is free on a quiet
//! machine is the one that suffers most on a busy one. That is the argument for
//! making commits fewer rather than for making them weaker.
//!
//! # What this does not do
//!
//! It never falls back. A platform that cannot make the call that was asked for
//! returns the error, because a fallback that quietly gives a weaker promise
//! than the caller asked for is the exact failure this module exists to prevent.
//! A caller willing to accept less says so by asking for less.

use std::fs::File;
use std::io;

/// How far a write has got when the sync returns.
///
/// Ordered from the strongest promise to the weakest. Not every platform has
/// three different calls to offer, and where it has fewer the weaker reaches
/// make the same call as the stronger one and [`call`](Self::call) says so, so
/// what gets printed is always what was really run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub enum Reach {
    /// The write is on the medium. It survives losing power at the instant the
    /// sync returned.
    ///
    /// This is the default, because it is the promise the commit documentation
    /// makes and a default that does not keep the promise is a bug that only
    /// shows up in an outage.
    #[default]
    Platter,
    /// The write has reached the device, which may be holding it in a cache
    /// that losing power empties.
    ///
    /// It survives the process dying and it survives the machine's kernel
    /// panicking, since neither of those touches the device. It does not
    /// survive the power going.
    Device,
    /// The write is ordered ahead of everything written after it, without
    /// waiting for it to reach anything.
    ///
    /// Nothing at all is promised about what is on the medium when this
    /// returns. What is promised is that if the write after it is on the
    /// medium, this one is too. That is enough for a log whose reader stops at
    /// the first record that does not check out, and it is much cheaper than
    /// waiting, so it is worth having for a caller that has thought about it.
    Ordered,
}

impl Reach {
    /// The name of the platform call this makes on this build.
    ///
    /// This is the string to print beside a commit latency. Two numbers with
    /// different strings beside them are not comparable, and that is the whole
    /// reason this function exists.
    #[must_use]
    pub const fn call(self) -> &'static str {
        platform::call(self)
    }

    /// One line saying what a write that this returned for survives.
    #[must_use]
    pub const fn promise(self) -> &'static str {
        match self {
            Self::Platter => "the power going",
            Self::Device => "the process dying, not the power going",
            Self::Ordered => "nothing, but nothing written after it reaches the medium first",
        }
    }
}

/// Puts what has been written where `reach` says, and no less.
///
/// # Errors
///
/// Returns whatever the platform call returned. There is no retry and no
/// fallback: a sync that failed is entitled to have thrown the writes away, and
/// asking again asks about writes the platform may no longer have.
pub fn sync(file: &File, reach: Reach) -> io::Result<()> {
    platform::sync(file, reach)
}

/// The same, for a write that also made the file longer.
///
/// The length is metadata and a data sync is entitled to leave it behind, so a
/// file that grew needs this instead. A store whose segment sits inside a file
/// that ends before it is a store that will not open.
///
/// This is never weaker than [`Reach::Device`], whatever was asked for, because
/// there is no call anywhere that carries a length and promises less. Where the
/// reach asks for more than that, it gets it.
///
/// It is not the call a commit latency is quoted from. A commit rewrites a
/// manifest slot that is already there and changes no length, so it goes
/// through [`sync`] and [`Reach::call`] names what it did.
///
/// # Errors
///
/// As [`sync`].
pub fn sync_all(file: &File, reach: Reach) -> io::Result<()> {
    platform::sync_all(file, reach)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd as _;

    use super::Reach;

    /// Ask the drive to empty its write cache, not just to take the write.
    const F_FULLFSYNC: i32 = 51;

    /// Order this write ahead of the ones after it, without waiting.
    const F_BARRIERFSYNC: i32 = 85;

    // SAFETY: the signature matches the platform's. `fcntl` is variadic and the
    // two commands used here take no third argument, so the calls below pass
    // none.
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }

    /// The name of the call each reach makes here.
    pub const fn call(reach: Reach) -> &'static str {
        match reach {
            Reach::Platter => "F_FULLFSYNC",
            Reach::Device => "fsync",
            Reach::Ordered => "F_BARRIERFSYNC",
        }
    }

    /// Syncs, by whichever of the three calls was asked for.
    pub fn sync(file: &File, reach: Reach) -> io::Result<()> {
        let command = match reach {
            Reach::Platter => F_FULLFSYNC,
            // The standard library's own call, which is `fsync` here. There is
            // no reason to declare it again.
            Reach::Device => return file.sync_data(),
            Reach::Ordered => F_BARRIERFSYNC,
        };
        // SAFETY: the descriptor is open and outlives the call, and neither
        // command takes an argument beyond the two passed.
        let outcome = unsafe { fcntl(file.as_raw_fd(), command) };
        if outcome == -1 {
            // A filesystem that does not support the command says so here.
            // Handing back the error is the point: the caller asked for a
            // promise this mount cannot make, and quietly making a weaker one
            // would be worse than failing.
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Syncs the length with the bytes, then reaches as far as was asked.
    pub fn sync_all(file: &File, reach: Reach) -> io::Result<()> {
        // First, because the drive cannot be asked to empty a cache holding a
        // write the operating system has not handed it yet.
        file.sync_all()?;
        if reach == Reach::Platter {
            sync(file, reach)
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::File;
    use std::io;

    use super::Reach;

    /// The name of the call every reach makes here.
    ///
    /// There is one call and it flushes the device cache, so asking for less
    /// than the strongest reach gets the strongest reach anyway. Saying so is
    /// better than pretending there was a choice.
    pub const fn call(_reach: Reach) -> &'static str {
        "FlushFileBuffers"
    }

    /// Syncs. The standard library's `sync_data` is `FlushFileBuffers` here.
    pub fn sync(file: &File, _reach: Reach) -> io::Result<()> {
        file.sync_data()
    }

    /// Syncs the length with the bytes. There is one call and this is it.
    pub fn sync_all(file: &File, _reach: Reach) -> io::Result<()> {
        file.sync_all()
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs::File;
    use std::io;

    use super::Reach;

    /// The name of the call every reach makes here.
    ///
    /// `fdatasync` asks the block layer for a cache flush on every filesystem
    /// this runs on with its default mount options, so it already reaches the
    /// medium and there is nothing stronger to ask for. There is also nothing
    /// weaker worth reaching for: the barrier that macOS exposes has no
    /// portable equivalent here.
    pub const fn call(_reach: Reach) -> &'static str {
        "fdatasync"
    }

    /// Syncs. The standard library's `sync_data` is `fdatasync` here.
    pub fn sync(file: &File, _reach: Reach) -> io::Result<()> {
        file.sync_data()
    }

    /// Syncs the length with the bytes. There is one call and this is it.
    pub fn sync_all(file: &File, _reach: Reach) -> io::Result<()> {
        file.sync_all()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform {
    use std::fs::File;
    use std::io;

    use super::Reach;

    /// The name of the call every reach makes here.
    ///
    /// The metadata goes too, which is more than a data sync needs, and it is
    /// the call whose name is the same on every remaining platform. Being able
    /// to print a name that is certainly true is worth more than saving the
    /// inode write on a platform nobody has measured yet.
    pub const fn call(_reach: Reach) -> &'static str {
        "fsync"
    }

    /// Syncs, metadata and all.
    pub fn sync(file: &File, _reach: Reach) -> io::Result<()> {
        file.sync_all()
    }

    /// Syncs the length with the bytes. There is one call and this is it.
    pub fn sync_all(file: &File, _reach: Reach) -> io::Result<()> {
        file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::{Reach, sync};

    /// A file to sync, in a directory that goes away with it.
    fn temporary(name: &str) -> (std::path::PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!("kura-durability-{name}"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("a file");
        (path, file)
    }

    #[test]
    fn every_reach_syncs_a_real_write() {
        use std::io::Write as _;
        for (index, reach) in [Reach::Platter, Reach::Device, Reach::Ordered]
            .into_iter()
            .enumerate()
        {
            let (path, mut file) = temporary(&format!("reach-{index}"));
            file.write_all(b"a write worth keeping").expect("written");
            sync(&file, reach).expect("the platform makes the call it named");
            drop(file);
            let back = std::fs::read(&path).expect("read back");
            assert_eq!(back, b"a write worth keeping");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn every_reach_names_a_call_and_a_promise() {
        for reach in [Reach::Platter, Reach::Device, Reach::Ordered] {
            assert!(!reach.call().is_empty(), "a reach with no call to name");
            assert!(!reach.promise().is_empty(), "a reach with no promise");
        }
    }

    #[test]
    fn the_default_is_the_one_that_survives_the_power_going() {
        assert_eq!(Reach::default(), Reach::Platter);
        assert_eq!(Reach::Platter.promise(), "the power going");
    }

    #[test]
    fn the_reaches_order_from_the_strongest_promise_to_the_weakest() {
        assert!(Reach::Platter < Reach::Device);
        assert!(Reach::Device < Reach::Ordered);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_three_reaches_are_three_different_calls_here() {
        assert_eq!(Reach::Platter.call(), "F_FULLFSYNC");
        assert_eq!(Reach::Device.call(), "fsync");
        assert_eq!(Reach::Ordered.call(), "F_BARRIERFSYNC");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_platform_with_one_call_names_it_for_every_reach() {
        assert_eq!(Reach::Platter.call(), Reach::Device.call());
        assert_eq!(Reach::Device.call(), Reach::Ordered.call());
    }
}
