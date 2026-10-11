//! `OrANS-Store` (`machine_id = 3`): the identity (verbatim) machine.
//!
//! Where `OrANS-NL2` (machine 1) expands every symbol into one core byte and
//! `OrANS-RANS0` (machine 2) compresses skewed sources, this third adapter is
//! deliberately trivial: it stores the source **verbatim** (`core = source`) and
//! reveals `core[cursor]`. It exists so the `auto` representation policy can
//! guarantee the base representation is never larger than raw — when the order-0
//! rANS model cannot shrink a source (uniform/random data), EKBLOOM stores the
//! source itself rather than an expanded encoding plus a model header.
//!
//! # Semantics
//!
//! Genesis `Omega_0 = (0, 0)`. One step reveals `core[cursor]`, sets
//! `state = u32::from(symbol)`, and advances `cursor` by one. The cursor equals
//! the logical position (`p_i = i`), exactly as machine 1, so the general
//! checkpoint / block / tile / bloom machinery (which rebases on the logical
//! position) applies through the machine-general code paths.
//!
//! `V = N` exactly: there is no model header and no expansion, so the base
//! representation is byte-for-byte the source.

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_STORE};
use crate::state::Snapshot;

/// The `OrANS-Store` machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Store;

/// The single process-wide `OrANS-Store` instance.
pub static STORE: Store = Store;

impl Store {
    /// Construct the machine.
    pub const fn new() -> Self {
        Store
    }
}

impl OrMachine for Store {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_STORE
    }

    #[inline]
    fn init(&self, _core: &[u8]) -> Result<Snapshot> {
        Ok(Snapshot::new(0, 0))
    }

    #[inline]
    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let p = omega.cursor;
        let s = *core.get(p as usize).ok_or(EkbloomError::CoreTooShort {
            needed: p + 1,
            available: core.len() as u64,
        })?;
        omega.state = u32::from(s);
        omega.cursor = p + 1;
        Ok(s)
    }

    #[inline]
    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        let p = omega.cursor;
        core.get(p as usize)
            .copied()
            .ok_or(EkbloomError::CoreTooShort {
                needed: p + 1,
                available: core.len() as u64,
            })
    }

    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        Ok(source.to_vec())
    }

    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        if logical_len > core.len() as u64 {
            return Err(EkbloomError::CoreTooShort {
                needed: logical_len,
                available: core.len() as u64,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_verbatim_for_every_shape() {
        let cases: [&[u8]; 6] = [
            b"",
            b"A",
            b"AB",
            b"hello, world",
            &[0u8; 300],
            &(0u8..=255).collect::<Vec<_>>(),
        ];
        for src in cases {
            let core = STORE.encode(src).unwrap();
            assert_eq!(core, src, "encode must be the identity");
            assert_eq!(STORE.decode(&core, src.len() as u64).unwrap(), src);
        }
    }

    #[test]
    fn cursor_equals_position() {
        let src = b"verbatim";
        let core = STORE.encode(src).unwrap();
        let mut omega = STORE.init(&core).unwrap();
        assert_eq!(omega, Snapshot::new(0, 0));
        for (i, &expect) in src.iter().enumerate() {
            assert_eq!(STORE.peek(&core, &omega).unwrap(), expect);
            assert_eq!(STORE.reveal(&core, &mut omega).unwrap(), expect);
            assert_eq!(omega.cursor, (i + 1) as u64, "cursor must equal position");
            assert_eq!(omega.state, u32::from(expect));
        }
    }

    #[test]
    fn check_core_rejects_over_declaration() {
        assert!(STORE.check_core(&[], 0).is_ok());
        assert!(STORE.check_core(&[1, 2, 3], 3).is_ok());
        assert!(STORE.check_core(&[1, 2, 3], 4).is_err());
    }
}
