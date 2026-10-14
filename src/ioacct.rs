//! Central I/O accounting for the file-backed reader.
//!
//! Every physical read the reader performs funnels through one [`ReadLedger`], so
//! no read path can silently bypass the counters. The tile path
//! ([`crate::reader`]'s `FileTiles::read_range`), the bounded encoded windows, the
//! decoder model ("dictionary") header load, the section/metadata reads and the
//! plain core reads all charge the same ledger through a [`LoggedFile`].
//!
//! The ledger keeps the *encoded core-window* bytes — the quantity the
//! bounded-window theorem bounds — separate from the total read-path I/O, so both
//! the theorem's counter and the whole read-path footprint can be read off one
//! interface.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{EkbloomError, Result};

/// The category of a physical read.
///
/// The class is recorded only so the encoded core-window bytes can be reported
/// apart from the total read-path I/O; every class is counted in the total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadClass {
    /// An encoded (renormalisation) core window: the bounded-window quantity.
    Encoded,
    /// Read-path payload: tile bytes, a path-bloom slice, a plain core window.
    Payload,
    /// Read-path metadata: a restart record, a section prefix, a directory.
    Metadata,
    /// A decoder model / dictionary header load.
    Dictionary,
}

/// The one accounting interface every physical read path charges.
///
/// Counters are monotone and relaxed (`Relaxed` ordering is sufficient: the
/// reader's counters are read for reporting, never used for synchronisation), and
/// recording only takes `&self`, so the hot read path stays lock-free.
#[derive(Debug, Default)]
pub struct ReadLedger {
    bytes: AtomicU64,
    syscalls: AtomicU64,
    encoded_bytes: AtomicU64,
}

impl ReadLedger {
    /// A fresh, zeroed ledger.
    #[inline]
    pub const fn new() -> ReadLedger {
        ReadLedger {
            bytes: AtomicU64::new(0),
            syscalls: AtomicU64::new(0),
            encoded_bytes: AtomicU64::new(0),
        }
    }

    /// Charge one physical read of `bytes` (one `pread`) in `class`.
    ///
    /// A zero-length read performs no syscall and is not charged.
    #[inline]
    pub fn record(&self, class: ReadClass, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.syscalls.fetch_add(1, Ordering::Relaxed);
        if class == ReadClass::Encoded {
            self.encoded_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Total read-path physical bytes charged.
    #[inline]
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Total read-path `pread` calls charged.
    #[inline]
    pub fn read_calls(&self) -> u64 {
        self.syscalls.load(Ordering::Relaxed)
    }

    /// Encoded core-window bytes charged (the bounded-window quantity).
    #[inline]
    pub fn encoded_bytes(&self) -> u64 {
        self.encoded_bytes.load(Ordering::Relaxed)
    }

    /// Reset every counter to zero.
    #[inline]
    pub fn reset(&self) {
        self.bytes.store(0, Ordering::Relaxed);
        self.syscalls.store(0, Ordering::Relaxed);
        self.encoded_bytes.store(0, Ordering::Relaxed);
    }
}

/// A bounds-checked positional reader bound to a [`ReadLedger`].
///
/// Every accessor validates the extent against the known file length (yielding a
/// clean typed error, never a panic) and then charges the ledger, so a physical
/// read cannot be performed through the reader without being counted.
pub struct LoggedFile<'a> {
    file: &'a File,
    total: u64,
    ledger: &'a ReadLedger,
}

impl<'a> LoggedFile<'a> {
    /// Bind `file` (of length `total`) to `ledger`.
    #[inline]
    pub fn new(file: &'a File, total: u64, ledger: &'a ReadLedger) -> LoggedFile<'a> {
        LoggedFile {
            file,
            total,
            ledger,
        }
    }

    /// Read exactly `buf.len()` bytes at `off` and charge the ledger in `class`.
    pub fn read_exact_at(&self, buf: &mut [u8], off: u64, class: ReadClass) -> Result<()> {
        let end = off
            .checked_add(buf.len() as u64)
            .ok_or(EkbloomError::Overflow {
                what: "file read extent",
            })?;
        if end > self.total {
            return Err(EkbloomError::Truncated {
                needed: end,
                available: self.total,
            });
        }
        self.file.read_exact_at(buf, off)?;
        self.ledger.record(class, buf.len() as u64);
        Ok(())
    }

    /// Read `len` bytes at `off` into a fresh buffer (bounds-checked).
    pub fn read_vec_at(&self, off: u64, len: u64, class: ReadClass) -> Result<Vec<u8>> {
        let n = usize::try_from(len).map_err(|_| EkbloomError::Overflow {
            what: "file read length",
        })?;
        let mut buf = vec![0u8; n];
        self.read_exact_at(&mut buf, off, class)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ledger_totals_and_reset() {
        let ledger = ReadLedger::new();
        assert_eq!(ledger.bytes(), 0);
        assert_eq!(ledger.read_calls(), 0);
        assert_eq!(ledger.encoded_bytes(), 0);
        ledger.record(ReadClass::Encoded, 100);
        ledger.record(ReadClass::Payload, 4096);
        ledger.record(ReadClass::Metadata, 12);
        ledger.record(ReadClass::Dictionary, 516);
        ledger.record(ReadClass::Payload, 0); // zero-length: no syscall
        assert_eq!(ledger.bytes(), 100 + 4096 + 12 + 516);
        assert_eq!(ledger.read_calls(), 4);
        assert_eq!(ledger.encoded_bytes(), 100);
        ledger.reset();
        assert_eq!(ledger.bytes(), 0);
        assert_eq!(ledger.read_calls(), 0);
        assert_eq!(ledger.encoded_bytes(), 0);
    }

    #[test]
    fn logged_file_charges_and_bounds() {
        let dir = std::env::temp_dir().join(format!("ekbloom-ioacct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.bin");
        let mut f = File::create(&path).unwrap();
        f.write_all(&(0u8..=255).collect::<Vec<u8>>()).unwrap();
        drop(f);
        let file = File::open(&path).unwrap();

        let ledger = ReadLedger::new();
        let logged = LoggedFile::new(&file, 256, &ledger);
        let mut buf = [0u8; 4];
        logged
            .read_exact_at(&mut buf, 8, ReadClass::Payload)
            .unwrap();
        assert_eq!(buf, [8, 9, 10, 11]);
        assert_eq!(ledger.bytes(), 4);
        assert_eq!(ledger.read_calls(), 1);
        // Out-of-range extents are clean typed errors, not panics or syscalls.
        assert!(matches!(
            logged.read_exact_at(&mut buf, 254, ReadClass::Payload),
            Err(EkbloomError::Truncated { .. })
        ));
        assert_eq!(ledger.read_calls(), 1);
        let v = logged.read_vec_at(0, 2, ReadClass::Encoded).unwrap();
        assert_eq!(v, vec![0, 1]);
        assert_eq!(ledger.encoded_bytes(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
