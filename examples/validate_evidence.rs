//! Validate every `evidence/**/*.json` against a strict JSON grammar.
//!
//! This is the guard the reviewer asked for: malformed evidence JSON must fail
//! the build, not silently publish. It is dependency-free (no `serde_json`) so it
//! runs in the pinned image, and it uses the same grammar a standard parser
//! accepts, so it never produces a false negative on valid evidence.
//!
//! Run with `sh scripts/validate-evidence.sh` (wired into
//! `scripts/container-reproduce.sh`); a malformed file makes this exit non-zero.

use std::error::Error;
use std::path::{Path, PathBuf};

/// Maximum nesting depth, a defensive bound against a crafted deeply nested file.
const MAX_DEPTH: usize = 256;

/// A minimal strict JSON validator over the UTF-8 bytes of a document.
struct Validator<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Validator<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Validator { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        match self.bump() {
            Some(b) if b == byte => Ok(()),
            Some(b) => Err(format!(
                "expected '{}' at byte {}, found '{}'",
                byte as char, self.pos, b as char
            )),
            None => Err(format!("expected '{}' at end of input", byte as char)),
        }
    }

    fn document(&mut self) -> Result<(), String> {
        self.skip_ws();
        self.value(0)?;
        self.skip_ws();
        if self.pos != self.bytes.len() {
            return Err(format!("trailing data at byte {}", self.pos));
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err(format!(
                "nesting deeper than {MAX_DEPTH} at byte {}",
                self.pos
            ));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string(),
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b) => Err(format!("unexpected '{}' at byte {}", b as char, self.pos)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn literal(&mut self, want: &str) -> Result<(), String> {
        let end = self.pos + want.len();
        if self.bytes.get(self.pos..end) == Some(want.as_bytes()) {
            self.pos = end;
            Ok(())
        } else {
            Err(format!("invalid literal at byte {}", self.pos))
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), String> {
        self.expect(b'{')?;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(());
        }
        loop {
            self.skip_ws();
            self.string()?; // key
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            self.value(depth + 1)?;
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => return Ok(()),
                Some(b) => {
                    return Err(format!(
                        "expected ',' or '}}' at byte {}, found '{}'",
                        self.pos, b as char
                    ))
                }
                None => return Err("unterminated object".to_string()),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), String> {
        self.expect(b'[')?;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(());
        }
        loop {
            self.skip_ws();
            self.value(depth + 1)?;
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => return Ok(()),
                Some(b) => {
                    return Err(format!(
                        "expected ',' or ']' at byte {}, found '{}'",
                        self.pos, b as char
                    ))
                }
                None => return Err("unterminated array".to_string()),
            }
        }
    }

    fn string(&mut self) -> Result<(), String> {
        self.expect(b'"')?;
        loop {
            match self.bump() {
                None => return Err("unterminated string".to_string()),
                Some(b'"') => return Ok(()),
                Some(b'\\') => match self.bump() {
                    Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {}
                    Some(b'u') => {
                        for _ in 0..4 {
                            match self.bump() {
                                Some(c) if c.is_ascii_hexdigit() => {}
                                _ => return Err("invalid \\u escape".to_string()),
                            }
                        }
                    }
                    Some(b) => {
                        return Err(format!(
                            "invalid escape '\\{}' at byte {}",
                            b as char, self.pos
                        ))
                    }
                    None => return Err("unterminated escape".to_string()),
                },
                // A raw control byte is not permitted inside a JSON string.
                Some(b) if b < 0x20 => {
                    return Err(format!("raw control byte at byte {}", self.pos))
                }
                Some(_) => {}
            }
        }
    }

    fn number(&mut self) -> Result<(), String> {
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: `0` or `[1-9][0-9]*`.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(format!("invalid number at byte {}", self.pos)),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("invalid fraction at byte {}", self.pos));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("invalid exponent at byte {}", self.pos));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        Ok(())
    }
}

/// Validate one JSON document.
fn validate_json(text: &str) -> Result<(), String> {
    Validator::new(text.as_bytes()).document()
}

/// Recursively collect every `*.json` path under `dir`.
fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_json(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn Error>> {
    let root = Path::new("evidence");
    let mut files = Vec::new();
    if root.is_dir() {
        collect_json(root, &mut files)?;
    }
    files.sort();
    if files.is_empty() {
        return Err("no evidence/**/*.json files found".into());
    }

    let mut failed = 0usize;
    for path in &files {
        let bytes = std::fs::read(path)?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(e) => {
                println!("FAIL {} (not UTF-8: {e})", path.display());
                failed += 1;
                continue;
            }
        };
        match validate_json(text) {
            Ok(()) => println!("OK   {}", path.display()),
            Err(e) => {
                println!("FAIL {} ({e})", path.display());
                failed += 1;
            }
        }
    }

    if failed != 0 {
        return Err(format!("{failed} malformed evidence JSON file(s)").into());
    }
    println!("validated {} evidence JSON file(s)", files.len());
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("validate_evidence: {e}");
        std::process::exit(1);
    }
}
