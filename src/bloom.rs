//! Shared bloom container types and certification records.
//!
//! This module holds the format-level vocabulary common to every bloom
//! construction (universal, contextual, regional, path):
//!
//! - [`BloomKind`] names the construction.
//! - [`Certification`] is the three-state certification verdict.
//! - [`Certificate`] is the self-describing, receipts-hashed evidence record.
//! - [`BloomShell`] is the generic manifested payload container.
//!
//! The machine-specific constructions (`bloom_global`, `bloom_context`,
//! `bloom_region`, `bloom_path`) build a [`BloomShell`] whose `payload` encodes
//! their entries; the shell carries the identifying metadata
//! (`kind`, `depth`, `generation`, `logical_len`, `entries`, `core_hash`) needed
//! to dispatch and cross-check it. See `docs/DESIGN.md` §3.
//!
//! All wire forms are little-endian and bounds-checked. Truncated input is
//! rejected with [`EkbloomError::Truncated`]; structurally inconsistent input is
//! rejected with [`EkbloomError::BadSection`]. Untrusted bytes are never
//! reinterpreted as native structs.

use crate::error::{EkbloomError, Result};

/// Bloom depths supported by the frozen contract: `D` in `{1, 2, 4, 8, 16, 32}`.
pub const SUPPORTED_DEPTHS: [u16; 6] = [1, 2, 4, 8, 16, 32];

/// Whether `depth` is one of [`SUPPORTED_DEPTHS`].
#[inline]
pub fn supported_depth(depth: u16) -> bool {
    SUPPORTED_DEPTHS.contains(&depth)
}

/// Which bloom construction a shell belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BloomKind {
    /// `T_D[x]`, keyed by the 16-bit machine state only.
    Universal,
    /// `T_D[x, c]`, keyed by state plus the next `D` core bytes.
    Contextual,
    /// `T_D^(reg)[x]`, keyed by state within a region.
    Regional,
    /// `P_D[j]`, keyed by block ordinal `j`; exact by construction.
    Path,
}

impl BloomKind {
    /// Stable, human-readable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            BloomKind::Universal => "universal",
            BloomKind::Contextual => "contextual",
            BloomKind::Regional => "regional",
            BloomKind::Path => "path",
        }
    }

    /// Stable wire code.
    pub fn to_u32(&self) -> u32 {
        match self {
            BloomKind::Universal => 0,
            BloomKind::Contextual => 1,
            BloomKind::Regional => 2,
            BloomKind::Path => 3,
        }
    }

    /// Decode a wire code, or `None` if unknown.
    pub fn from_u32(v: u32) -> Option<BloomKind> {
        match v {
            0 => Some(BloomKind::Universal),
            1 => Some(BloomKind::Contextual),
            2 => Some(BloomKind::Regional),
            3 => Some(BloomKind::Path),
            _ => None,
        }
    }
}

/// Certification verdict for a bloom. Only [`Certification::Certified`] blooms
/// may be used without fallback on the normal read path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Certification {
    /// Built but not yet certified.
    Unverified,
    /// Proven exact over its declared key domain.
    Certified,
    /// Proven insufficient (a counterexample exists).
    Rejected,
}

impl Certification {
    /// Stable, human-readable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            Certification::Unverified => "unverified",
            Certification::Certified => "certified",
            Certification::Rejected => "rejected",
        }
    }

    /// Stable wire code.
    pub fn to_u32(&self) -> u32 {
        match self {
            Certification::Unverified => 0,
            Certification::Certified => 1,
            Certification::Rejected => 2,
        }
    }

    /// Decode a wire code, or `None` if unknown.
    pub fn from_u32(v: u32) -> Option<Certification> {
        match v {
            0 => Some(Certification::Unverified),
            1 => Some(Certification::Certified),
            2 => Some(Certification::Rejected),
            _ => None,
        }
    }
}

/// A self-describing certification record.
///
/// The `receipt_hash` is a BLAKE3 digest over every other field in canonical
/// order (see [`Certificate::receipt_bytes`]); it is recomputed on
/// [`Certificate::to_bytes`] and re-verified on [`Certificate::from_bytes`], so
/// any tampering of a stored certificate is detected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    /// Which construction this certifies.
    pub kind: BloomKind,
    /// Bloom depth `D`.
    pub depth: u16,
    /// Key domain, e.g. `"x16"` or `"x16++cD"`.
    pub key_domain: String,
    /// Context definition, e.g. `"next-D-core-bytes"`.
    pub context_def: String,
    /// Commit generation the bloom was built for.
    pub generation: u64,
    /// Machine identifier the bloom was built for.
    pub machine_id: u32,
    /// BLAKE3 hash of the authoritative `core`.
    pub core_hash: [u8; 32],
    /// BLAKE3 hash of the manifested bloom content.
    pub bloom_hash: [u8; 32],
    /// Verification method, e.g. `"exhaustive-2^16"`.
    pub method: String,
    /// Domain actually verified, e.g. `"all-65536-states"`.
    pub verified_domain: String,
    /// Receipt digest over the other fields.
    pub receipt_hash: [u8; 32],
    /// Certification verdict.
    pub status: Certification,
}

impl Certificate {
    /// Canonical serialization of every field except `receipt_hash`.
    ///
    /// Layout (little-endian): kind `u32`, depth `u16`, machine_id `u32`,
    /// generation `u64`, status `u32`, then four length-prefixed (`u64` length)
    /// UTF-8 strings (`key_domain`, `context_def`, `method`, `verified_domain`),
    /// then `core_hash` (32) and `bloom_hash` (32).
    pub fn receipt_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.kind.to_u32().to_le_bytes());
        out.extend_from_slice(&self.depth.to_le_bytes());
        out.extend_from_slice(&self.machine_id.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.status.to_u32().to_le_bytes());
        push_str(&mut out, &self.key_domain);
        push_str(&mut out, &self.context_def);
        push_str(&mut out, &self.method);
        push_str(&mut out, &self.verified_domain);
        out.extend_from_slice(&self.core_hash);
        out.extend_from_slice(&self.bloom_hash);
        out
    }

    /// BLAKE3 receipt digest over [`Certificate::receipt_bytes`].
    pub fn compute_receipt_hash(&self) -> [u8; 32] {
        *blake3::hash(&self.receipt_bytes()).as_bytes()
    }

    /// Serialize to the exact wire layout: [`Certificate::receipt_bytes`]
    /// followed by the freshly computed `receipt_hash` (32 bytes).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.receipt_bytes();
        out.extend_from_slice(&self.compute_receipt_hash());
        out
    }

    /// Parse the exact wire layout produced by [`Certificate::to_bytes`].
    ///
    /// Rejects truncation with [`EkbloomError::Truncated`], unknown enum codes,
    /// invalid UTF-8, trailing bytes, and a mismatched `receipt_hash` with
    /// [`EkbloomError::BadSection`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Certificate> {
        let mut r = Reader::new(bytes);
        let kind =
            BloomKind::from_u32(r.u32()?).ok_or_else(|| bad("unknown certificate bloom kind"))?;
        let depth = r.u16()?;
        let machine_id = r.u32()?;
        let generation = r.u64()?;
        let status =
            Certification::from_u32(r.u32()?).ok_or_else(|| bad("unknown certificate status"))?;
        let key_domain = r.string()?;
        let context_def = r.string()?;
        let method = r.string()?;
        let verified_domain = r.string()?;
        let core_hash = r.arr32()?;
        let bloom_hash = r.arr32()?;
        let receipt_hash = r.arr32()?;
        if r.remaining() != 0 {
            return Err(bad("trailing bytes after certificate"));
        }
        let cert = Certificate {
            kind,
            depth,
            key_domain,
            context_def,
            generation,
            machine_id,
            core_hash,
            bloom_hash,
            method,
            verified_domain,
            receipt_hash,
            status,
        };
        if cert.compute_receipt_hash() != receipt_hash {
            return Err(bad("certificate receipt hash mismatch"));
        }
        Ok(cert)
    }
}

/// A generic manifested bloom payload container.
///
/// `payload` holds the construction-specific entry encoding; the surrounding
/// fields identify and cross-check it. `physical_bytes` is the manifested
/// payload volume. `content_hash` is a BLAKE3 digest over the full wire form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BloomShell {
    /// Which construction this payload encodes.
    pub kind: BloomKind,
    /// Bloom depth `D`.
    pub depth: u16,
    /// Commit generation this bloom belongs to.
    pub generation: u64,
    /// Number of logical symbols covered.
    pub logical_len: u64,
    /// Number of entries (e.g. blocks for a path bloom).
    pub entries: u64,
    /// Construction-specific encoded entries.
    pub payload: Vec<u8>,
    /// Certification verdict.
    pub status: Certification,
    /// BLAKE3 hash of the authoritative `core`.
    pub core_hash: [u8; 32],
}

/// Fixed wire-header size of a [`BloomShell`]: kind `u32`, depth `u16`,
/// generation `u64`, logical_len `u64`, entries `u64`, status `u32`,
/// core_hash `[u8; 32]`, payload_len `u64`.
const SHELL_HEADER_LEN: usize = 4 + 2 + 8 + 8 + 8 + 4 + 32 + 8;

impl BloomShell {
    /// Manifested payload volume in bytes.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.payload.len() as u64
    }

    /// BLAKE3 digest over the full wire form (see [`BloomShell::to_bytes`]).
    pub fn content_hash(&self) -> [u8; 32] {
        *blake3::hash(&self.to_bytes()).as_bytes()
    }

    /// Serialize: `kind u32`, `depth u16`, `generation u64`, `logical_len u64`,
    /// `entries u64`, `status u32`, `core_hash [32]`, `payload_len u64`, then the
    /// payload bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SHELL_HEADER_LEN.saturating_add(self.payload.len()));
        out.extend_from_slice(&self.kind.to_u32().to_le_bytes());
        out.extend_from_slice(&self.depth.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.logical_len.to_le_bytes());
        out.extend_from_slice(&self.entries.to_le_bytes());
        out.extend_from_slice(&self.status.to_u32().to_le_bytes());
        out.extend_from_slice(&self.core_hash);
        out.extend_from_slice(&(self.payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse the exact wire layout produced by [`BloomShell::to_bytes`].
    ///
    /// Rejects truncation with [`EkbloomError::Truncated`], unknown enum codes,
    /// a payload length that disagrees with the available bytes, and trailing
    /// bytes with [`EkbloomError::BadSection`].
    pub fn from_bytes(bytes: &[u8]) -> Result<BloomShell> {
        let mut r = Reader::new(bytes);
        let kind = BloomKind::from_u32(r.u32()?).ok_or_else(|| bad("unknown bloom shell kind"))?;
        let depth = r.u16()?;
        let generation = r.u64()?;
        let logical_len = r.u64()?;
        let entries = r.u64()?;
        let status =
            Certification::from_u32(r.u32()?).ok_or_else(|| bad("unknown bloom shell status"))?;
        let core_hash = r.arr32()?;
        let payload_len = r.u64()?;
        let payload_len = usize::try_from(payload_len).map_err(|_| EkbloomError::Overflow {
            what: "bloom shell payload length",
        })?;
        if r.remaining() < payload_len {
            let consumed = (bytes.len() - r.remaining()) as u64;
            return Err(EkbloomError::Truncated {
                needed: consumed + payload_len as u64,
                available: bytes.len() as u64,
            });
        }
        if r.remaining() != payload_len {
            return Err(bad(
                "bloom shell payload length does not match available bytes",
            ));
        }
        let payload = r.take(payload_len)?.to_vec();
        Ok(BloomShell {
            kind,
            depth,
            generation,
            logical_len,
            entries,
            payload,
            status,
            core_hash,
        })
    }
}

/// Pack one transition-bloom entry: `D` symbol bytes followed by a
/// little-endian `u32` successor state.
#[inline]
pub fn pack_block(symbols: &[u8], next_state: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(symbols.len().saturating_add(4));
    out.extend_from_slice(symbols);
    out.extend_from_slice(&next_state.to_le_bytes());
    out
}

/// Inverse of [`pack_block`]: `D` is inferred from `bytes.len() - 4`.
///
/// Rejects fewer than 4 bytes with [`EkbloomError::Truncated`].
#[inline]
pub fn unpack_block(bytes: &[u8]) -> Result<(Vec<u8>, u32)> {
    if bytes.len() < 4 {
        return Err(EkbloomError::Truncated {
            needed: 4,
            available: bytes.len() as u64,
        });
    }
    let split = bytes.len() - 4;
    let symbols = bytes[..split].to_vec();
    let state = u32::from_le_bytes([
        bytes[split],
        bytes[split + 1],
        bytes[split + 2],
        bytes[split + 3],
    ]);
    Ok((symbols, state))
}

/// Append a `u64`-length-prefixed UTF-8 string.
fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Construct a structural [`EkbloomError::BadSection`].
fn bad(reason: &str) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.to_string(),
    }
}

/// A bounds-checked little-endian byte reader for parsing wire forms.
struct Reader<'a> {
    bytes: &'a [u8],
    off: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, off: 0 }
    }

    #[inline]
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.off)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.off.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "wire parse offset",
        })?;
        let slice = self
            .bytes
            .get(self.off..end)
            .ok_or(EkbloomError::Truncated {
                needed: end as u64,
                available: self.bytes.len() as u64,
            })?;
        self.off = end;
        Ok(slice)
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn arr32(&mut self) -> Result<[u8; 32]> {
        let b = self.take(32)?;
        let mut a = [0u8; 32];
        a.copy_from_slice(b);
        Ok(a)
    }

    fn string(&mut self) -> Result<String> {
        let n = self.u64()?;
        let n = usize::try_from(n).map_err(|_| EkbloomError::Overflow {
            what: "wire string length",
        })?;
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| bad("wire string is not valid utf-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_certificate() -> Certificate {
        let mut cert = Certificate {
            kind: BloomKind::Path,
            depth: 4,
            key_domain: "block-ordinal".to_string(),
            context_def: "j-in-0..nblocks".to_string(),
            generation: 9,
            machine_id: 1,
            core_hash: [0x11; 32],
            bloom_hash: [0x22; 32],
            method: "by-construction".to_string(),
            verified_domain: "all-blocks".to_string(),
            receipt_hash: [0u8; 32],
            status: Certification::Certified,
        };
        cert.receipt_hash = cert.compute_receipt_hash();
        cert
    }

    fn sample_shell() -> BloomShell {
        BloomShell {
            kind: BloomKind::Path,
            depth: 4,
            generation: 9,
            logical_len: 10,
            entries: 3,
            payload: (0..22u8).collect(),
            status: Certification::Unverified,
            core_hash: [0x33; 32],
        }
    }

    #[test]
    fn kind_wire_roundtrip() {
        let kinds = [
            BloomKind::Universal,
            BloomKind::Contextual,
            BloomKind::Regional,
            BloomKind::Path,
        ];
        for k in kinds {
            assert_eq!(BloomKind::from_u32(k.to_u32()), Some(k));
            assert!(!k.as_str().is_empty());
        }
        assert_eq!(BloomKind::from_u32(99), None);
        assert_eq!(BloomKind::Path.as_str(), "path");
    }

    #[test]
    fn certification_wire_roundtrip() {
        let states = [
            Certification::Unverified,
            Certification::Certified,
            Certification::Rejected,
        ];
        for s in states {
            assert_eq!(Certification::from_u32(s.to_u32()), Some(s));
            assert!(!s.as_str().is_empty());
        }
        assert_eq!(Certification::from_u32(7), None);
    }

    #[test]
    fn pack_block_roundtrip_and_truncation() {
        let (syms, state) = unpack_block(&pack_block(&[1, 2, 3, 4], 0xDEAD_BEEF)).unwrap();
        assert_eq!(syms, vec![1, 2, 3, 4]);
        assert_eq!(state, 0xDEAD_BEEF);

        // Empty-symbol block (h=0) is legal: just a successor state.
        let (syms, state) = unpack_block(&pack_block(&[], 7)).unwrap();
        assert!(syms.is_empty());
        assert_eq!(state, 7);

        for cut in 0..4 {
            assert!(matches!(
                unpack_block(&[0u8; 4][..cut]),
                Err(EkbloomError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn shell_roundtrip_and_truncation() {
        let shell = sample_shell();
        assert_eq!(shell.physical_bytes(), 22);
        let bytes = shell.to_bytes();
        assert_eq!(bytes.len(), SHELL_HEADER_LEN + 22);
        let back = BloomShell::from_bytes(&bytes).unwrap();
        assert_eq!(back, shell);
        assert_eq!(back.to_bytes(), bytes);
        assert_eq!(back.content_hash(), shell.content_hash());

        for cut in 0..bytes.len() {
            assert!(
                BloomShell::from_bytes(&bytes[..cut]).is_err(),
                "expected shell parse error at cut {cut}"
            );
        }

        // Payload-length disagreement with trailing bytes is a structural error.
        let mut long = bytes.clone();
        long.push(0);
        assert!(matches!(
            BloomShell::from_bytes(&long),
            Err(EkbloomError::BadSection { .. })
        ));

        // Unknown kind code.
        let mut k = bytes.clone();
        k[0..4].copy_from_slice(&77u32.to_le_bytes());
        assert!(matches!(
            BloomShell::from_bytes(&k),
            Err(EkbloomError::BadSection { .. })
        ));
    }

    #[test]
    fn certificate_roundtrip_and_receipt_tamper() {
        let cert = sample_certificate();
        let bytes = cert.to_bytes();
        let back = Certificate::from_bytes(&bytes).unwrap();
        assert_eq!(back, cert);
        assert_eq!(back.to_bytes(), bytes);
        assert_eq!(back.receipt_hash, cert.compute_receipt_hash());

        for cut in 0..bytes.len() {
            assert!(
                Certificate::from_bytes(&bytes[..cut]).is_err(),
                "expected certificate parse error at cut {cut}"
            );
        }

        // Tampering the trailing receipt digest must be detected.
        let mut t = bytes.clone();
        let last = t.len() - 1;
        t[last] ^= 0xFF;
        assert!(matches!(
            Certificate::from_bytes(&t),
            Err(EkbloomError::BadSection { .. })
        ));

        // Tampering a hashed field also breaks the receipt.
        let mut t2 = bytes.clone();
        t2[0] ^= 0x01;
        assert!(matches!(
            Certificate::from_bytes(&t2),
            Err(EkbloomError::BadSection { .. })
        ));
    }
}
