//! Exact path bloom `P_D[j]` — the correctness reference and speed ceiling.
//!
//! The path bloom keys by **block ordinal** `j` rather than by machine state, so
//! it is exact by construction: block `j` stores the authoritative symbols for
//! logical positions `j*D .. j*D + D` together with the machine state that
//! resumes just after them. It cannot collide because the key (a block index)
//! carries no ambiguity — it is a manifested, not inferred, projection.
//!
//! # Layout
//!
//! ```text
//! PathBloom {
//!     depth:       u16,   // D in {1,2,4,8,16,32}
//!     logical_len: u64,   // N
//!     hot:  Vec<u8>,      // one byte per logical symbol (the output surface)
//!     cold: Vec<u32>,     // one successor state per block
//! }
//! ```
//!
//! `blocks = ceil(N / D)`. For block `j < blocks - 1`, `cold[j] = x_{D*(j+1)}`.
//! For the final (possibly partial) block, `cold[blocks-1] = x_N`. A partial
//! final block exposes only its real symbols; padding is never returned as
//! logical content.
//!
//! # Shell payload
//!
//! [`PathBloom::build`] returns a [`BloomShell`] with `kind = Path`; the payload
//! is `depth u16` ++ `logical_len u64` ++ `nblocks u64` ++ `hot` (`N` bytes) ++
//! `cold` (`nblocks * u32` little-endian). [`PathBloom::from_shell`] parses this
//! layout strictly.

use crate::bloom::{supported_depth, BloomKind, BloomShell, Certification};
use crate::error::{EkbloomError, Result};
use crate::machine::{state_log, OrMachine, NL2};

/// Fixed size of the path payload prefix: `depth u16`, `logical_len u64`,
/// `nblocks u64`.
const PAYLOAD_PREFIX_LEN: usize = 2 + 8 + 8;

/// An exact, block-keyed path bloom.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathBloom {
    /// Bloom depth `D`.
    pub depth: u16,
    /// Number of logical symbols `N`.
    pub logical_len: u64,
    /// One authoritative symbol per logical position (`N` bytes).
    pub hot: Vec<u8>,
    /// One successor state per block: `x_{D*(j+1)}`, or `x_N` for the final
    /// partial block (`ceil(N / D)` entries).
    pub cold: Vec<u32>,
}

impl PathBloom {
    /// Build the exact path bloom for `core` producing `logical_len` symbols at
    /// `depth`, and return it as a `Path` [`BloomShell`] tagged with `generation`
    /// and the BLAKE3 hash of `core`.
    ///
    /// Rejects an unsupported `depth` with [`EkbloomError::BadDepth`] and a short
    /// `core` with [`EkbloomError::CoreTooShort`].
    pub fn build(core: &[u8], logical_len: u64, depth: u16, generation: u64) -> Result<BloomShell> {
        if !supported_depth(depth) {
            return Err(EkbloomError::BadDepth {
                depth: u32::from(depth),
            });
        }
        // Authoritative decode: `hot` is the manifested output surface.
        let hot = NL2.decode(core, logical_len)?;
        // Cold successors come from the machine state log.
        let states = state_log(core, logical_len)?;

        let d = u64::from(depth);
        let nblocks = logical_len.div_ceil(d);
        let nblocks_usize = usize::try_from(nblocks).map_err(|_| EkbloomError::Overflow {
            what: "path bloom block count",
        })?;
        let mut cold: Vec<u32> = Vec::with_capacity(nblocks_usize);
        for j in 0..nblocks {
            let end = d.saturating_mul(j + 1).min(logical_len);
            cold.push(states[end as usize]);
        }

        let bloom = PathBloom {
            depth,
            logical_len,
            hot,
            cold,
        };
        Ok(bloom.to_shell(generation, *blake3::hash(core).as_bytes()))
    }

    /// The authoritative symbol at logical position `pos`.
    ///
    /// Errors with [`EkbloomError::OutOfBounds`] when `pos >= logical_len`.
    pub fn symbol_at(&self, pos: u64) -> Result<u8> {
        let idx = usize::try_from(pos).map_err(|_| EkbloomError::OutOfBounds {
            offset: pos,
            length: 1,
            logical_len: self.logical_len,
        })?;
        self.hot.get(idx).copied().ok_or(EkbloomError::OutOfBounds {
            offset: pos,
            length: 1,
            logical_len: self.logical_len,
        })
    }

    /// The symbols of block `block` (`D` of them, or fewer for a partial final
    /// block) and its successor state.
    ///
    /// Errors with [`EkbloomError::OutOfBounds`] when `block` is past the last
    /// block.
    pub fn block_at(&self, block: u64) -> Result<(Vec<u8>, u32)> {
        let d = u64::from(self.depth);
        let start = block.checked_mul(d).ok_or(EkbloomError::Overflow {
            what: "path bloom block offset",
        })?;
        if start >= self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset: start,
                length: d,
                logical_len: self.logical_len,
            });
        }
        let end = start.saturating_add(d).min(self.logical_len);
        let start_usize = usize::try_from(start).map_err(|_| EkbloomError::Overflow {
            what: "path bloom block offset",
        })?;
        let end_usize = usize::try_from(end).map_err(|_| EkbloomError::Overflow {
            what: "path bloom block offset",
        })?;
        let symbols = self.hot[start_usize..end_usize].to_vec();
        let idx = usize::try_from(block).map_err(|_| EkbloomError::OutOfBounds {
            offset: start,
            length: d,
            logical_len: self.logical_len,
        })?;
        let successor = *self.cold.get(idx).ok_or(EkbloomError::OutOfBounds {
            offset: start,
            length: d,
            logical_len: self.logical_len,
        })?;
        Ok((symbols, successor))
    }

    /// Manifested size: `hot.len() + 4 * cold.len()`.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        (self.hot.len() as u64).saturating_add((self.cold.len() as u64).saturating_mul(4))
    }

    /// Wrap this bloom into a `Path` [`BloomShell`] with the given `generation`
    /// and `core_hash`.
    pub fn to_shell(&self, generation: u64, core_hash: [u8; 32]) -> BloomShell {
        let nblocks = self.cold.len() as u64;
        let mut payload =
            Vec::with_capacity(PAYLOAD_PREFIX_LEN + self.hot.len() + self.cold.len() * 4);
        payload.extend_from_slice(&self.depth.to_le_bytes());
        payload.extend_from_slice(&self.logical_len.to_le_bytes());
        payload.extend_from_slice(&nblocks.to_le_bytes());
        payload.extend_from_slice(&self.hot);
        for &x in &self.cold {
            payload.extend_from_slice(&x.to_le_bytes());
        }
        BloomShell {
            kind: BloomKind::Path,
            depth: self.depth,
            generation,
            logical_len: self.logical_len,
            entries: nblocks,
            payload,
            status: Certification::Unverified,
            core_hash,
        }
    }

    /// Parse a `Path` [`BloomShell`] produced by [`PathBloom::build`] or
    /// [`PathBloom::to_shell`].
    ///
    /// Strictly validates the kind, depth, cross-checks the payload header
    /// against the shell metadata, and requires the payload length to match
    /// exactly. Rejects truncation with [`EkbloomError::Truncated`], an
    /// unsupported depth with [`EkbloomError::BadDepth`], and structural
    /// inconsistency with [`EkbloomError::BadSection`].
    pub fn from_shell(shell: &BloomShell) -> Result<PathBloom> {
        if shell.kind != BloomKind::Path {
            return Err(EkbloomError::BadSection {
                reason: format!("expected a path bloom, got {}", shell.kind.as_str()),
            });
        }
        if !supported_depth(shell.depth) {
            return Err(EkbloomError::BadDepth {
                depth: u32::from(shell.depth),
            });
        }
        let p = &shell.payload;
        if p.len() < PAYLOAD_PREFIX_LEN {
            return Err(EkbloomError::Truncated {
                needed: PAYLOAD_PREFIX_LEN as u64,
                available: p.len() as u64,
            });
        }
        let depth = u16::from_le_bytes([p[0], p[1]]);
        if depth != shell.depth {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "payload depth {depth} disagrees with shell depth {}",
                    shell.depth
                ),
            });
        }
        let logical_len = u64::from_le_bytes([p[2], p[3], p[4], p[5], p[6], p[7], p[8], p[9]]);
        if logical_len != shell.logical_len {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "payload logical_len {logical_len} disagrees with shell {}",
                    shell.logical_len
                ),
            });
        }
        let nblocks = u64::from_le_bytes([p[10], p[11], p[12], p[13], p[14], p[15], p[16], p[17]]);
        let expected_nblocks = logical_len.div_ceil(u64::from(depth));
        if nblocks != expected_nblocks {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "payload nblocks {nblocks} inconsistent with logical_len {logical_len} at depth {depth}"
                ),
            });
        }
        if shell.entries != nblocks {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "shell entries {} disagrees with payload nblocks {nblocks}",
                    shell.entries
                ),
            });
        }
        let hot_len = usize::try_from(logical_len).map_err(|_| EkbloomError::Overflow {
            what: "path bloom logical length",
        })?;
        let cold_len = usize::try_from(nblocks).map_err(|_| EkbloomError::Overflow {
            what: "path bloom block count",
        })?;
        let cold_bytes = cold_len.checked_mul(4).ok_or(EkbloomError::Overflow {
            what: "path bloom cold bytes",
        })?;
        let need = PAYLOAD_PREFIX_LEN
            .checked_add(hot_len)
            .and_then(|n| n.checked_add(cold_bytes))
            .ok_or(EkbloomError::Overflow {
                what: "path bloom payload length",
            })?;
        if p.len() < need {
            return Err(EkbloomError::Truncated {
                needed: need as u64,
                available: p.len() as u64,
            });
        }
        if p.len() != need {
            return Err(EkbloomError::BadSection {
                reason: "path payload has trailing bytes".to_string(),
            });
        }
        let hot = p[PAYLOAD_PREFIX_LEN..PAYLOAD_PREFIX_LEN + hot_len].to_vec();
        let mut cold = Vec::with_capacity(cold_len);
        let mut off = PAYLOAD_PREFIX_LEN + hot_len;
        for _ in 0..cold_len {
            cold.push(u32::from_le_bytes([
                p[off],
                p[off + 1],
                p[off + 2],
                p[off + 3],
            ]));
            off += 4;
        }
        Ok(PathBloom {
            depth,
            logical_len,
            hot,
            cold,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: u64, len: usize) -> Vec<u8> {
        let mut s = seed;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push((s >> 33) as u8);
        }
        out
    }

    #[test]
    fn build_rejects_bad_depth() {
        let core = NL2.encode(b"abc").unwrap();
        for d in [0u16, 3, 5, 6, 7, 9, 31, 33, 64] {
            assert!(matches!(
                PathBloom::build(&core, 3, d, 0),
                Err(EkbloomError::BadDepth { .. })
            ));
        }
    }

    #[test]
    fn symbols_blocks_and_size_agree_with_machine() {
        let src = lcg(0x00C0_FFEE, 130);
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let decoded = NL2.decode(&core, n).unwrap();
        let logs = state_log(&core, n).unwrap();

        for &d in &[1u16, 2, 4, 8, 16, 32] {
            let shell = PathBloom::build(&core, n, d, 4).unwrap();
            assert_eq!(shell.kind, BloomKind::Path);
            assert_eq!(shell.depth, d);
            assert_eq!(shell.logical_len, n);
            let pb = PathBloom::from_shell(&shell).unwrap();
            let dd = u64::from(d);
            let nblocks = n.div_ceil(dd);
            assert_eq!(pb.cold.len() as u64, nblocks);
            assert_eq!(pb.physical_bytes(), n + 4 * nblocks);
            // The shell's manifested payload adds the fixed path prefix.
            assert_eq!(
                shell.physical_bytes(),
                pb.physical_bytes() + PAYLOAD_PREFIX_LEN as u64
            );

            for pos in 0..n {
                assert_eq!(pb.symbol_at(pos).unwrap(), decoded[pos as usize]);
            }
            for j in 0..nblocks {
                let (syms, succ) = pb.block_at(j).unwrap();
                let start = j * dd;
                let end = (start + dd).min(n);
                assert_eq!(syms.len() as u64, end - start);
                for (k, s) in syms.iter().enumerate() {
                    assert_eq!(*s, decoded[start as usize + k]);
                }
                assert_eq!(succ, logs[end as usize]);
            }
            assert!(pb.symbol_at(n).is_err());
            assert!(pb.block_at(nblocks).is_err());
        }
    }

    #[test]
    fn shell_and_payload_roundtrip_strict() {
        let core = NL2.encode(&lcg(7, 100)).unwrap();
        let shell = PathBloom::build(&core, 100, 8, 6).unwrap();
        let bytes = shell.to_bytes();
        let back = BloomShell::from_bytes(&bytes).unwrap();
        assert_eq!(back, shell);
        let pb = PathBloom::from_shell(&back).unwrap();

        // A non-path shell is rejected.
        let mut wrong = shell.clone();
        wrong.kind = BloomKind::Universal;
        assert!(matches!(
            PathBloom::from_shell(&wrong),
            Err(EkbloomError::BadSection { .. })
        ));

        // Truncating the payload is rejected.
        let mut short = shell.clone();
        short.payload.truncate(shell.payload.len() - 1);
        assert!(PathBloom::from_shell(&short).is_err());

        // Removing a byte while keeping a valid nblocks makes the payload length
        // no longer match; parsing is rejected.
        let mut trimmed = shell.clone();
        trimmed.payload.remove(PAYLOAD_PREFIX_LEN);
        assert!(PathBloom::from_shell(&trimmed).is_err());

        // The parsed bloom preserves the size invariant.
        assert_eq!(pb.physical_bytes(), 100 + 4 * (100u64.div_ceil(8)));
    }

    #[test]
    fn empty_and_partial_blocks() {
        // Empty object: no blocks, no symbols.
        let shell = PathBloom::build(&[], 0, 4, 0).unwrap();
        let pb = PathBloom::from_shell(&shell).unwrap();
        assert_eq!(pb.physical_bytes(), 0);
        assert!(pb.cold.is_empty());
        assert!(pb.hot.is_empty());
        assert!(pb.symbol_at(0).is_err());
        assert!(pb.block_at(0).is_err());

        // Non-divisible length: final block is partial and exposes no padding.
        let core = NL2.encode(b"abcdefg").unwrap();
        let pb = PathBloom::from_shell(&PathBloom::build(&core, 7, 4, 0).unwrap()).unwrap();
        let (syms, _) = pb.block_at(1).unwrap();
        assert_eq!(syms, b"efg".to_vec());
    }
}
