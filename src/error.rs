//! Error type for EKBLOOM.

use std::fmt;

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, EkbloomError>;

/// All EKBLOOM failure modes. Every failure is explicit; nothing is silently ignored.
#[derive(Debug)]
pub enum EkbloomError {
    /// Underlying I/O failure.
    Io(std::io::Error),
    /// File does not begin with the `EKBLOOM\0` magic.
    BadMagic,
    /// Format version not supported by this build.
    UnsupportedVersion { major: u16, minor: u16 },
    /// A declared length or offset exceeded the actually available bytes.
    Truncated { needed: u64, available: u64 },
    /// A section or record failed its checksum.
    ChecksumMismatch { section: &'static str },
    /// A section header or body was structurally invalid.
    BadSection { reason: String },
    /// Checked integer arithmetic overflowed.
    Overflow { what: &'static str },
    /// A bloom depth that is unsupported or out of range.
    BadDepth { depth: u32 },
    /// A checkpoint interval that is unsupported or zero where zero is invalid.
    BadInterval { interval: u32 },
    /// A machine identifier with no registered machine.
    UnknownMachine { id: u32 },
    /// The core was too short to produce the declared number of symbols.
    CoreTooShort { needed: u64, available: u64 },
    /// The requested logical range lies outside the object.
    OutOfBounds {
        offset: u64,
        length: u64,
        logical_len: u64,
    },
    /// A bloom key was found insufficient (a counterexample exists).
    KeyInsufficient { detail: String },
    /// A structure was used on the fast path without certification.
    NotCertified { what: &'static str },
    /// A committed generation failed validation.
    CorruptGeneration { generation: u64, reason: String },
    /// A requested allocation exceeded the configured resource policy.
    AllocationTooLarge { requested: u64, limit: u64 },
    /// Corrupt transition data risked nontermination; aborted.
    NonTermination,
    /// A caller argument was invalid.
    InvalidArgument { what: String },
    /// Recovery could not find any valid committed generation.
    RecoveryFailed { reason: String },
}

impl fmt::Display for EkbloomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EkbloomError::Io(e) => write!(f, "io error: {e}"),
            EkbloomError::BadMagic => write!(f, "bad magic: not an EKBLOOM file"),
            EkbloomError::UnsupportedVersion { major, minor } => {
                write!(f, "unsupported format version {major}.{minor}")
            }
            EkbloomError::Truncated { needed, available } => {
                write!(f, "truncated: needed {needed} bytes, {available} available")
            }
            EkbloomError::ChecksumMismatch { section } => {
                write!(f, "checksum mismatch in section {section}")
            }
            EkbloomError::BadSection { reason } => write!(f, "bad section: {reason}"),
            EkbloomError::Overflow { what } => write!(f, "integer overflow: {what}"),
            EkbloomError::BadDepth { depth } => write!(f, "unsupported bloom depth {depth}"),
            EkbloomError::BadInterval { interval } => {
                write!(f, "unsupported checkpoint interval {interval}")
            }
            EkbloomError::UnknownMachine { id } => write!(f, "unknown machine id {id}"),
            EkbloomError::CoreTooShort { needed, available } => {
                write!(f, "core too short: need {needed} bytes, have {available}")
            }
            EkbloomError::OutOfBounds {
                offset,
                length,
                logical_len,
            } => write!(
                f,
                "read out of bounds: offset {offset}, length {length}, logical_len {logical_len}"
            ),
            EkbloomError::KeyInsufficient { detail } => {
                write!(f, "bloom key insufficient: {detail}")
            }
            EkbloomError::NotCertified { what } => write!(f, "{what} is not certified"),
            EkbloomError::CorruptGeneration { generation, reason } => {
                write!(f, "generation {generation} corrupt: {reason}")
            }
            EkbloomError::AllocationTooLarge { requested, limit } => {
                write!(f, "allocation {requested} exceeds limit {limit}")
            }
            EkbloomError::NonTermination => {
                write!(f, "aborted: transition data risks nontermination")
            }
            EkbloomError::InvalidArgument { what } => write!(f, "invalid argument: {what}"),
            EkbloomError::RecoveryFailed { reason } => write!(f, "recovery failed: {reason}"),
        }
    }
}

impl std::error::Error for EkbloomError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EkbloomError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for EkbloomError {
    fn from(e: std::io::Error) -> Self {
        EkbloomError::Io(e)
    }
}
