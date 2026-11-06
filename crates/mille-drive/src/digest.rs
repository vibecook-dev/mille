//! `Blake3`, an object's name (DR-L3), and the one layout both ends of a pull
//! share: `objects/<first 2 hex>/<64 hex>`.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{DriveError, DriveErrorCode, Result};

/// Hex characters in a digest's spelling.
pub const BLAKE3_HEX_LEN: usize = 64;

/// The read size for hashing and copying.
pub(crate) const IO_CHUNK: usize = 64 * 1024;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// A blake3 digest. Its one spelling is 64 lowercase hex characters: `Display`
/// writes it, `FromStr` accepts nothing else, and serde carries it as that
/// string.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Blake3([u8; 32]);

impl Blake3 {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The digest of `data`.
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    /// The digest and length of a file, read in chunks.
    pub fn of_file(path: &Path) -> io::Result<(Self, u64)> {
        let mut file = File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; IO_CHUNK];
        let mut total = 0u64;
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            hasher.update(&buf[..n]);
            total += n as u64;
        }
        Ok((Self(*hasher.finalize().as_bytes()), total))
    }

    pub(crate) fn from_hasher(hasher: &blake3::Hasher) -> Self {
        Self(*hasher.finalize().as_bytes())
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(BLAKE3_HEX_LEN);
        for byte in self.0 {
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        out
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

impl FromStr for Blake3 {
    type Err = DriveError;

    fn from_str(s: &str) -> Result<Self> {
        let bytes = s.as_bytes();
        if bytes.len() != BLAKE3_HEX_LEN {
            return Err(DriveError::new(
                DriveErrorCode::Invalid,
                format!("a blake3 digest is 64 lowercase hex characters, got {s:?}"),
            ));
        }
        let mut out = [0u8; 32];
        for (i, pair) in bytes.chunks_exact(2).enumerate() {
            let (Some(hi), Some(lo)) = (nibble(pair[0]), nibble(pair[1])) else {
                return Err(DriveError::new(
                    DriveErrorCode::Invalid,
                    format!("a blake3 digest is lowercase hex only, got {s:?}"),
                ));
            };
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

impl fmt::Display for Blake3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Blake3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Blake3({})", self.to_hex())
    }
}

impl Serialize for Blake3 {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Blake3 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse::<Blake3>()
            .map_err(|err| serde::de::Error::custom(err.message().to_owned()))
    }
}

/// An object's path relative to the drive's objects root:
/// `<first 2 hex>/<64 hex>`. Stable and public — a puller composes the
/// holder's `objects_root()` with this to name the file it asks for — so
/// changing it is a format change (README, "On disk").
pub fn object_relpath(digest: &Blake3) -> PathBuf {
    let hex = digest.to_hex();
    let mut path = PathBuf::from(&hex[..2]);
    path.push(&hex);
    path
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn hex_round_trips_and_is_the_display() {
        let digest = Blake3::of(b"hello drive");
        let hex = digest.to_hex();
        assert_eq!(hex.len(), BLAKE3_HEX_LEN);
        assert_eq!(hex, digest.to_string());
        assert_eq!(hex, blake3::hash(b"hello drive").to_hex().as_str());
        assert_eq!(hex.parse::<Blake3>().unwrap(), digest);
    }

    #[test]
    fn only_64_lowercase_hex_parses() {
        let good = "a".repeat(64);
        assert!(good.parse::<Blake3>().is_ok());
        for bad in [
            String::new(),
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            format!("{}g", "a".repeat(63)),
            format!("b3:{}", "a".repeat(61)),
        ] {
            let err = bad.parse::<Blake3>().unwrap_err();
            assert_eq!(err.code(), DriveErrorCode::Invalid, "{bad:?}");
        }
    }

    #[test]
    fn object_relpath_is_two_hex_then_the_whole_digest() {
        let digest = Blake3::of(b"x");
        let hex = digest.to_hex();
        let rel = object_relpath(&digest);
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(parts, vec![hex[..2].to_string(), hex.clone()]);
    }

    #[test]
    fn a_streamed_file_hash_equals_the_hash_of_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        // Larger than one read chunk, and not a multiple of it.
        let data: Vec<u8> = (0..(IO_CHUNK * 3 + 17)).map(|i| (i % 251) as u8).collect();
        File::create(&path).unwrap().write_all(&data).unwrap();
        let (digest, len) = Blake3::of_file(&path).unwrap();
        assert_eq!(digest, Blake3::of(&data));
        assert_eq!(len, data.len() as u64);
    }

    #[test]
    fn serde_carries_the_hex_spelling() {
        let digest = Blake3::of(b"y");
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json, format!("\"{}\"", digest.to_hex()));
        let back: Blake3 = serde_json::from_str(&json).unwrap();
        assert_eq!(back, digest);
        assert!(serde_json::from_str::<Blake3>("\"nope\"").is_err());
    }
}
