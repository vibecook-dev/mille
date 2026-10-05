//! Core entry types. Mirrors the `api.d.ts` public surface so NAPI
//! marshaling stays mechanical.

use std::sync::atomic::{AtomicU64, Ordering};

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use crate::error::FxError;

/// JS `Number.MAX_SAFE_INTEGER` ceiling. EntryIds must stay below this.
pub const ENTRY_ID_MAX: u64 = 1u64 << 53;

/// Session-scoped monotonic id. Newtype around `u64`; serialized as a plain u64
/// (NAPI layer coerces to a JS `number` under the 2^53 cap).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub u64);

impl EntryId {
    /// Allocate the next id from `counter`.
    ///
    /// Returns `Err(FxError::EntryIdExhausted)` once the counter reaches the
    /// 2^53 JS-safe-integer ceiling. PLAN 13.5 defers recycling; here we
    /// refuse gracefully instead of panicking.
    pub fn alloc_from(counter: &AtomicU64) -> Result<EntryId, FxError> {
        let raw = counter.fetch_add(1, Ordering::Relaxed);
        if raw >= ENTRY_ID_MAX {
            // Saturate so subsequent calls also fail (don't wrap back below cap).
            counter.store(ENTRY_ID_MAX, Ordering::Relaxed);
            return Err(FxError::EntryIdExhausted);
        }
        Ok(EntryId(raw))
    }

    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Kind of filesystem entry. `#[repr(u8)]` so it can cross NAPI as a small int.
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum EntryKind {
    File = 0,
    Directory = 1,
    Symlink = 2,
    Unknown = 3,
    /// A configured workspace root that is currently missing or inaccessible.
    ///
    /// The root entry and its identity remain published while known
    /// descendants are evicted. A later successful reconciliation restores
    /// the concrete directory/symlink kind without allocating a new id.
    Unavailable = 4,
}

impl EntryKind {
    /// The kind a `#[repr(u8)]` discriminant names, as it crosses NAPI;
    /// `None` for a value no kind uses.
    pub fn from_discriminant(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::File),
            1 => Some(Self::Directory),
            2 => Some(Self::Symlink),
            3 => Some(Self::Unknown),
            4 => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// One row in the flat-array model. Fields match `api.d.ts` Entry exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    #[serde(default)]
    pub parent_id: Option<EntryId>,
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ms: i64,
    pub ctime_ms: i64,
    #[serde(default)]
    pub symlink_target_is_dir: Option<bool>,
    #[serde(default)]
    pub path_segments: Option<Vec<String>>,
    /// Ignored by repository-owned rules (`.gitignore`, `.ignore`,
    /// `.rgignore`). Configured explorer excludes are tracked separately so
    /// they can be removed at runtime without losing this provenance.
    pub is_ignored: bool,
    pub is_readonly: bool,
    pub is_hidden: bool,
    /// Appended to the serialized record so format-v1 entries can still
    /// deserialize with the default and reach the resume version gate.
    #[serde(default)]
    pub is_excluded: bool,
}

impl Entry {
    #[inline]
    pub fn is_ignored_or_excluded(&self) -> bool {
        self.is_ignored || self.is_excluded
    }
}

bitflags! {
    /// Provider capability mask. Mirrors `api.d.ts` Capability const enum.
    #[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Default, Serialize, Deserialize)]
    pub struct Capability: u32 {
        const NONE           = 0;
        const READ_WRITE     = 1 << 0;
        const CASE_SENSITIVE = 1 << 1;
        const READONLY       = 1 << 2;
        const TRASH          = 1 << 3;
        const ATOMIC_WRITE   = 1 << 4;
        const WATCH          = 1 << 5;
        const STREAM         = 1 << 6;
        const CLONE          = 1 << 7;
        const REALPATH       = 1 << 8;
        const FILE_LOCK      = 1 << 9;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_from_produces_monotonic_ids() {
        let counter = AtomicU64::new(0);
        let a = EntryId::alloc_from(&counter).unwrap();
        let b = EntryId::alloc_from(&counter).unwrap();
        let c = EntryId::alloc_from(&counter).unwrap();
        assert_eq!(a.raw(), 0);
        assert_eq!(b.raw(), 1);
        assert_eq!(c.raw(), 2);
        assert!(a < b && b < c);
    }

    #[test]
    fn alloc_from_refuses_past_2_pow_53() {
        // Seed just under the ceiling; next two calls straddle it.
        let counter = AtomicU64::new(ENTRY_ID_MAX - 1);
        let last = EntryId::alloc_from(&counter).expect("one id left");
        assert_eq!(last.raw(), ENTRY_ID_MAX - 1);

        let err = EntryId::alloc_from(&counter).expect_err("should refuse");
        assert!(matches!(err, FxError::EntryIdExhausted));

        // Subsequent calls continue to fail (saturation, not wrap).
        let err2 = EntryId::alloc_from(&counter).expect_err("still exhausted");
        assert!(matches!(err2, FxError::EntryIdExhausted));
    }

    #[test]
    fn capability_bitmask_operations() {
        let caps = Capability::READ_WRITE | Capability::WATCH | Capability::STREAM;
        assert!(caps.contains(Capability::READ_WRITE));
        assert!(caps.contains(Capability::WATCH));
        assert!(!caps.contains(Capability::TRASH));

        let union = Capability::READ_WRITE.union(Capability::TRASH);
        assert!(union.contains(Capability::READ_WRITE));
        assert!(union.contains(Capability::TRASH));

        assert_eq!(Capability::NONE.bits(), 0);
        assert_eq!(Capability::FILE_LOCK.bits(), 1 << 9);
    }

    #[test]
    fn capability_roundtrips_via_serde() {
        let caps = Capability::READ_WRITE | Capability::CASE_SENSITIVE | Capability::REALPATH;
        let encoded = bincode::serde::encode_to_vec(caps, bincode::config::standard()).unwrap();
        let (decoded, _): (Capability, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(caps, decoded);
    }

    #[test]
    fn entry_roundtrips_via_bincode() {
        let entry = Entry {
            id: EntryId(42),
            parent_id: Some(EntryId(7)),
            name: "README.md".to_string(),
            kind: EntryKind::File,
            size: 1234,
            mtime_ms: 1_700_000_000_000,
            ctime_ms: 1_699_000_000_000,
            symlink_target_is_dir: None,
            path_segments: Some(vec!["a".into(), "b".into(), "README.md".into()]),
            is_ignored: false,
            is_excluded: false,
            is_readonly: false,
            is_hidden: false,
        };
        let encoded = bincode::serde::encode_to_vec(&entry, bincode::config::standard()).unwrap();
        let (decoded, _): (Entry, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(entry, decoded);
    }

    #[test]
    fn entry_kind_repr_u8() {
        assert_eq!(EntryKind::File as u8, 0);
        assert_eq!(EntryKind::Directory as u8, 1);
        assert_eq!(EntryKind::Symlink as u8, 2);
        assert_eq!(EntryKind::Unknown as u8, 3);
        assert_eq!(EntryKind::Unavailable as u8, 4);
    }

    #[test]
    fn entry_kind_round_trips_its_discriminant() {
        for kind in [
            EntryKind::File,
            EntryKind::Directory,
            EntryKind::Symlink,
            EntryKind::Unknown,
            EntryKind::Unavailable,
        ] {
            assert_eq!(EntryKind::from_discriminant(kind as u32), Some(kind));
        }
        assert_eq!(EntryKind::from_discriminant(5), None);
        assert_eq!(EntryKind::from_discriminant(u32::MAX), None);
    }
}
