//! Shared trained dictionaries for small records (`OrANS-DICT`, machine 9).
//!
//! A per-record order-0 model (machine 2) carries a 512-byte frequency table plus a
//! 4-byte state in **every** core. For a 100-byte record that header is five times
//! the payload, so machine 2 is catastrophic on small records even when their
//! *content* is highly skewed. This module factors the model out: a **dictionary**
//! is trained once over a corpus of small records and stored once, and each record
//! is then a compact header-less core that merely **references** it.
//!
//! # Model
//!
//! A [`Dictionary`] holds an order-0 byte model: 256 little-endian `u16`
//! frequencies summing to `M = 4096`, each `>= 1`, scaled from the corpus byte
//! counts by the same largest-remainder method machine 2 uses for a single source.
//! It carries a 16-byte **content identity** derived by BLAKE3 over the version and
//! the frequencies, so a dictionary can be validated against a stored id and a
//! tampered table is refused rather than decoded.
//!
//! # Core layout
//!
//! `to_bytes()` (540 bytes total):
//!
//! ```text
//! [magic "EKBDICT\0": 8][version u16][reserved u16][id: 16][freq: 256 * u16 LE]
//! ```
//!
//! `encode(record)` is a compact core that references the dictionary and carries
//! **no** model header:
//!
//! ```text
//! [logical_len: u32 LE][initial_state: u32 LE][renorm bytes, reversed]
//! ```
//!
//! `decode(core)` uses the shared model and the length carried in the core.

use std::collections::HashMap;
// The registry lock is the crate's third shared-mutable structure after the
// `OrFs` index and the `ObjectCache` maps. Under `--cfg loom` the very same
// `register`/`lookup`/`registered`/`unregister` code is compiled against loom's
// instrumented `RwLock` so `tests/loom_dictionary.rs` exhaustively explores its
// publication/lookup interleavings; production builds use `std::sync`. No
// behaviour differs.
#[cfg(loom)]
use loom::sync::RwLock;
#[cfg(not(loom))]
use std::sync::OnceLock;
#[cfg(not(loom))]
use std::sync::RwLock;

use crate::error::{EkbloomError, Result};
use crate::machine_rans::{valid_state, RansContext, M, MODEL_BYTES, RANS_L, SCALE_BITS, SYMBOLS};
use crate::state::Snapshot;

/// Magic prefix of a serialized dictionary.
pub const DICT_MAGIC: [u8; 8] = *b"EKBDICT\0";
/// Serialized dictionary record version.
pub const DICT_VERSION: u16 = 1;
/// Fixed dictionary header: magic ++ version ++ reserved ++ content id.
pub const DICT_HEADER_BYTES: usize = 8 + 2 + 2 + 16;
/// Serialized model length: 256 little-endian `u16` frequencies.
pub const DICT_MODEL_BYTES: usize = SYMBOLS * 2;
/// Total serialized dictionary length (`540`).
pub const DICT_BYTES: usize = DICT_HEADER_BYTES + DICT_MODEL_BYTES;
/// Per-record core framing produced by [`Dictionary::encode`]:
/// `logical_len u32` ++ `initial_state u32`.
pub const DICT_RECORD_HEADER_BYTES: usize = 8;
/// Domain separator for the content-derived dictionary id.
const DICT_ID_DOMAIN: &[u8] = b"ekbloom.dictionary-id.v1";

/// A trained shared order-0 model referenced by many small records.
///
/// Two `Dictionary` values are equal exactly when their content (and therefore
/// their id) is equal, so it derives `Eq` on the identity and the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dictionary {
    id: [u8; 16],
    freq: [u16; SYMBOLS],
}

/// Train a shared order-0 dictionary from a corpus of records.
///
/// Byte counts are aggregated over every record, clamped to `>= 1` per symbol, and
/// scaled to `M = 4096` by the largest-remainder method (identical to machine 2's
/// single-source model), so `freq[s] >= 1` and `sum(freq) == M` for any corpus —
/// including the empty corpus, whose model is uniform (`16` per symbol).
pub fn train_dictionary(records: &[&[u8]]) -> Result<Dictionary> {
    let mut count = [0u64; SYMBOLS];
    for record in records {
        for &b in record.iter() {
            let slot = &mut count[b as usize];
            *slot = slot.checked_add(1).ok_or(EkbloomError::Overflow {
                what: "dictionary byte count",
            })?;
        }
    }
    let freq = scale_counts(&count);
    Ok(Dictionary {
        id: compute_id(&freq),
        freq,
    })
}

/// Scale aggregated byte counts to a `M = 4096` frequency table by the
/// largest-remainder method (each symbol reserved `1`, the rest distributed
/// proportionally to the clamped counts, remainder to the largest fractions).
fn scale_counts(count: &[u64; SYMBOLS]) -> [u16; SYMBOLS] {
    let clamped: [u64; SYMBOLS] = std::array::from_fn(|i| count[i].max(1));
    let total: u64 = clamped.iter().sum();
    let budget: u64 = u64::from(M) - SYMBOLS as u64;

    let mut rem = [0u64; SYMBOLS];
    let mut frac = [0u64; SYMBOLS];
    let mut assigned: u64 = 0;
    for (i, &c) in clamped.iter().enumerate() {
        let num = u128::from(c) * u128::from(budget);
        rem[i] = (num / u128::from(total)) as u64;
        frac[i] = (num % u128::from(total)) as u64;
        assigned += rem[i];
    }
    // `total >= SYMBOLS`, so the fractional parts leave at most `SYMBOLS - 1`
    // units to hand out.
    let leftover = (budget - assigned) as usize;
    let mut order: Vec<usize> = (0..SYMBOLS).collect();
    order.sort_by(|&a, &b| frac[b].cmp(&frac[a]).then(a.cmp(&b)));
    for &i in order.iter().take(leftover) {
        rem[i] += 1;
    }

    std::array::from_fn(|i| (rem[i] + 1) as u16)
}

/// Derive the content id from the version and the frequency table.
fn compute_id(freq: &[u16; SYMBOLS]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DICT_ID_DOMAIN);
    hasher.update(&DICT_VERSION.to_le_bytes());
    for f in freq.iter() {
        hasher.update(&f.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    id
}

/// Prefix sums of a frequency table (`cum[s]` is the number of slots before `s`).
fn cumulate(freq: &[u16; SYMBOLS]) -> [u32; SYMBOLS] {
    let mut cum = [0u32; SYMBOLS];
    let mut acc = 0u32;
    for (c, &f) in cum.iter_mut().zip(freq.iter()) {
        *c = acc;
        acc += u32::from(f);
    }
    cum
}

impl Dictionary {
    /// Train a shared order-0 dictionary from a corpus of records (one line).
    ///
    /// A thin alias for [`train_dictionary`], mirroring `zstd::dict::from_samples`:
    /// train once over a corpus, then encode every record against the shared model.
    /// The companion "use" half is [`create_with_dict`](crate::create_with_dict) or
    /// [`Dictionary::encode_record`].
    ///
    /// ```
    /// # fn main() -> ekbloom::Result<()> {
    /// let corpus: [&[u8]; 2] = [b"user=alice|region=us-east-1", b"user=bob|region=eu-west-1"];
    /// let dict = ekbloom::Dictionary::train(&corpus)?;
    /// let body = ekbloom::create_with_dict(&dict, b"user=carol|region=us-east-1")?;
    /// assert_eq!(body.machine_id, ekbloom::MACHINE_DICT);
    /// # Ok(())
    /// # }
    /// ```
    pub fn train(records: &[&[u8]]) -> Result<Dictionary> {
        train_dictionary(records)
    }

    /// The content identity (`blake3("ekbloom.dictionary-id.v1" ++ version ++ freq)[0..16]`).
    #[inline]
    pub fn id(&self) -> [u8; 16] {
        self.id
    }

    /// Total physical size attributable to the shared dictionary (`540` bytes).
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        DICT_BYTES as u64
    }

    /// Serialize the dictionary (`540` bytes).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DICT_BYTES);
        out.extend_from_slice(&DICT_MAGIC);
        out.extend_from_slice(&DICT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.id);
        for f in self.freq.iter() {
            out.extend_from_slice(&f.to_le_bytes());
        }
        out
    }

    /// Parse and validate a serialized dictionary.
    ///
    /// Rejects a truncated/over-long record, bad magic, an unknown version, a table
    /// that does not sum to `M` or contains a zero, and a content-id mismatch (the
    /// recomputed id must equal the stored one).
    pub fn from_bytes(bytes: &[u8]) -> Result<Dictionary> {
        if bytes.len() < DICT_BYTES {
            return Err(EkbloomError::Truncated {
                needed: DICT_BYTES as u64,
                available: bytes.len() as u64,
            });
        }
        if bytes.len() > DICT_BYTES {
            return Err(EkbloomError::BadSection {
                reason: "dictionary record has trailing bytes".to_string(),
            });
        }
        if bytes[..DICT_MAGIC.len()] != DICT_MAGIC {
            return Err(EkbloomError::BadSection {
                reason: "bad dictionary magic".to_string(),
            });
        }
        let version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if version != DICT_VERSION {
            return Err(EkbloomError::BadSection {
                reason: format!("unsupported dictionary version {version}"),
            });
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&bytes[12..28]);
        let mut freq = [0u16; SYMBOLS];
        let mut sum = 0u32;
        for (i, slot) in freq.iter_mut().enumerate() {
            let o = DICT_HEADER_BYTES + i * 2;
            let v = u16::from_le_bytes([bytes[o], bytes[o + 1]]);
            *slot = v;
            sum += u32::from(v);
        }
        if sum != M {
            return Err(EkbloomError::BadSection {
                reason: format!("dictionary frequencies sum to {sum}, expected {M}"),
            });
        }
        if freq.contains(&0) {
            return Err(EkbloomError::BadSection {
                reason: "dictionary contains a zero frequency".to_string(),
            });
        }
        if compute_id(&freq) != id {
            return Err(EkbloomError::BadSection {
                reason: "dictionary content id mismatch".to_string(),
            });
        }
        Ok(Dictionary { id, freq })
    }

    /// A 516-byte machine-2 model header (`freq` ++ `initial_state`) built from this
    /// dictionary, so the shared model can drive a validated [`RansContext`].
    pub(crate) fn model_header(&self, initial_state: u32) -> Vec<u8> {
        let mut header = vec![0u8; MODEL_BYTES + 4];
        for (i, &f) in self.freq.iter().enumerate() {
            header[i * 2..i * 2 + 2].copy_from_slice(&f.to_le_bytes());
        }
        header[MODEL_BYTES..MODEL_BYTES + 4].copy_from_slice(&initial_state.to_le_bytes());
        header
    }

    /// Encode `record` as a compact core that references this dictionary: no model
    /// header, just `[logical_len u32][initial_state u32][renorm bytes, reversed]`.
    pub fn encode(&self, record: &[u8]) -> Result<Vec<u8>> {
        let logical_len = u32::try_from(record.len()).map_err(|_| EkbloomError::Overflow {
            what: "dictionary record length",
        })?;
        let cum = cumulate(&self.freq);
        let mut x: u32 = RANS_L;
        let mut emitted: Vec<u8> = Vec::new();
        for &sym in record.iter().rev() {
            let f = u64::from(self.freq[sym as usize]);
            let c = u64::from(cum[sym as usize]);
            let x_max = u64::from((RANS_L >> SCALE_BITS) << 8) * f;
            while u64::from(x) >= x_max {
                emitted.push((x & 0xFF) as u8);
                x >>= 8;
            }
            let xu = u64::from(x);
            x = (((xu / f) << SCALE_BITS) + (xu % f) + c) as u32;
        }

        let mut core = Vec::with_capacity(DICT_RECORD_HEADER_BYTES.saturating_add(emitted.len()));
        core.extend_from_slice(&logical_len.to_le_bytes());
        core.extend_from_slice(&x.to_le_bytes());
        core.extend(emitted.iter().rev());
        Ok(core)
    }

    /// Encode `record` as a complete machine-9 (`OrANS-DICT`) core that
    /// **references** this dictionary, registering it so a reader can resolve the
    /// id — the one-line "use" half of the ergonomic path
    /// (`Dictionary::train(&corpus)?.encode_record(record)?`).
    ///
    /// Unlike [`Dictionary::encode`] (which returns the bare header-less record
    /// core), the returned bytes are a full machine-9 core:
    /// `[dict_id: 16][dict_len = 0]` ++ the header-less record core. Decode it with
    /// [`crate::machine_dict::DictMachine`] (registered as
    /// [`MACHINE_DICT`](crate::MACHINE_DICT)) or drop it into a body via
    /// [`create_with_dict`](crate::create_with_dict).
    pub fn encode_record(&self, record: &[u8]) -> Result<Vec<u8>> {
        register(self)?;
        let core = self.encode(record)?;
        Ok(crate::machine_dict::frame_reference(self, &core))
    }

    /// Decode a core produced by [`Dictionary::encode`] against this dictionary.
    ///
    /// The core must begin with the declared symbol count and a legal coder state;
    /// a forged state or a truncated core is a typed error, never a silent wrong
    /// byte.
    pub fn decode(&self, core: &[u8]) -> Result<Vec<u8>> {
        if core.len() < DICT_RECORD_HEADER_BYTES {
            return Err(EkbloomError::Truncated {
                needed: DICT_RECORD_HEADER_BYTES as u64,
                available: core.len() as u64,
            });
        }
        let logical_len = u64::from(u32::from_le_bytes([core[0], core[1], core[2], core[3]]));
        let initial_state = u32::from_le_bytes([core[4], core[5], core[6], core[7]]);
        if !valid_state(initial_state) {
            return Err(EkbloomError::BadSection {
                reason: format!("dictionary record state {initial_state} out of range"),
            });
        }
        crate::resource::checked_default(logical_len)?;
        let header = self.model_header(initial_state);
        let ctx = RansContext::from_core(&header)?;
        let mut omega = Snapshot::new(initial_state, DICT_RECORD_HEADER_BYTES as u64);
        let mut out: Vec<u8> = Vec::new();
        ctx.decode_into(core, &mut omega, logical_len, &mut out)?;
        Ok(out)
    }
}

/// Process-wide registry of trained dictionaries, so a machine-9 core may
/// **reference** a dictionary by content id instead of embedding it.
///
/// The one-time cell initialisation is **not** part of the modelled protocol:
/// loom 0.7.2's `sync` module has no `OnceLock`, and a `std::sync::OnceLock`
/// cannot be reset between the schedules `loom::model` re-runs its closure for.
/// A normal build keeps the plain `OnceLock` cell below; the loom build uses a
/// resettable slot (see `reset_registry_for_model`) and models the `RwLock`
/// protocol around the map, which is the shared state that actually matters.
#[cfg(not(loom))]
static REGISTRY: OnceLock<RwLock<HashMap<[u8; 16], Dictionary>>> = OnceLock::new();

#[cfg(not(loom))]
fn registry() -> &'static RwLock<HashMap<[u8; 16], Dictionary>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Loom-only slot holding the registry installed by
/// [`reset_registry_for_model`]. It is a plain `std::sync::Mutex` guarding only
/// a pointer; it is deliberately **not** a loom primitive and is uncontended
/// during the modelled region (it is installed before any thread is spawned), so
/// it is not part of the protocol under test.
#[cfg(loom)]
static REGISTRY_SLOT: std::sync::Mutex<Option<&'static RwLock<HashMap<[u8; 16], Dictionary>>>> =
    std::sync::Mutex::new(None);

#[cfg(loom)]
fn registry() -> &'static RwLock<HashMap<[u8; 16], Dictionary>> {
    let mut slot = REGISTRY_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    match *slot {
        Some(lock) => lock,
        None => {
            let lock: &'static RwLock<HashMap<[u8; 16], Dictionary>> =
                Box::leak(Box::new(RwLock::new(HashMap::new())));
            *slot = Some(lock);
            lock
        }
    }
}

/// Install a fresh, empty registry for the schedule about to run (loom only).
///
/// `loom::model` re-runs its closure once per explored schedule, and a
/// `loom::sync::RwLock` belongs to the execution that created it, so a registry
/// carried across schedules would be stale. Call this at the **top of every
/// model closure** so each schedule starts from a clean registry. This item does
/// not exist in an ordinary build; the previous registry is abandoned to process
/// teardown (the loom build is test-only and short-lived).
#[cfg(loom)]
pub fn reset_registry_for_model() {
    let lock: &'static RwLock<HashMap<[u8; 16], Dictionary>> =
        Box::leak(Box::new(RwLock::new(HashMap::new())));
    let mut slot = REGISTRY_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(lock);
}

/// Register `dict` so a machine-9 core may reference it by [`Dictionary::id`].
///
/// Registering the same id twice is idempotent (the content is identical by
/// construction). Returns a typed error only if the registry lock is poisoned.
pub fn register(dict: &Dictionary) -> Result<()> {
    let mut map = registry().write().map_err(|_| EkbloomError::BadSection {
        reason: "dictionary registry lock poisoned".to_string(),
    })?;
    map.insert(dict.id, dict.clone());
    Ok(())
}

/// Resolve a registered dictionary by content id, if present.
pub fn lookup(id: &[u8; 16]) -> Option<Dictionary> {
    match registry().read() {
        Ok(map) => map.get(id).cloned(),
        Err(_) => None,
    }
}

/// Whether `id` is currently registered.
pub fn registered(id: &[u8; 16]) -> bool {
    matches!(registry().read(), Ok(map) if map.contains_key(id))
}

/// Remove a registered dictionary, returning whether it was present (tests only).
pub fn unregister(id: &[u8; 16]) -> bool {
    match registry().write() {
        Ok(mut map) => map.remove(id).is_some(),
        Err(_) => false,
    }
}
