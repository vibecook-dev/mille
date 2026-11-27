//! A point-in-time directory listing used while JS applies its normal sort,
//! visibility and offset rules. Only the returned page enters the live cache.
use std::path::PathBuf;

use mille_core::{Entry, EntryId, ErrorCode, FxError};
use napi::Result;
use napi_derive::napi;

use crate::error::fx_error_to_napi;
use crate::explorer::FileExplorer;
use crate::types::EntryJs;
use crate::watch_runtime::ReconcileOutcome;

pub(crate) fn cache_full() -> napi::Error {
    fx_error_to_napi(FxError::Io {
        code: ErrorCode::ENOSPC,
        path: PathBuf::new(),
        source: std::io::Error::other(
            "maxCachedEntries has no room for this path and all workspace roots",
        ),
    })
}

#[napi]
pub struct DirectoryPage {
    explorer: FileExplorer,
    parent: EntryId,
    version: u64,
    rows: Vec<(PathBuf, Entry)>,
}

impl DirectoryPage {
    pub(crate) fn new(
        explorer: FileExplorer,
        parent: EntryId,
        rows: Vec<(PathBuf, Entry)>,
    ) -> Self {
        let version = explorer.store.tree_version();
        Self {
            explorer,
            parent,
            version,
            rows,
        }
    }
}

#[napi]
impl DirectoryPage {
    /// The temporary ids are row indexes and stay private to the JS wrapper.
    #[napi(catch_unwind)]
    pub fn candidates(&self) -> Vec<EntryJs> {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, (_, entry))| {
                let mut row = EntryJs::from_core(entry);
                row.id = i as i64;
                row
            })
            .collect()
    }

    /// A concurrent modification makes the wrapper reopen the listing before
    /// publishing anything. This prevents a deleted row from being revived.
    #[napi(catch_unwind)]
    pub fn retain(&self, indices: Vec<u32>) -> Result<Option<Vec<EntryJs>>> {
        let _guard = self.explorer.policy_gate.lock();
        let store = &self.explorer.store;
        if store.tree_version() != self.version {
            return Ok(None);
        }
        let capacity = store.child_capacity(self.parent);
        if capacity == 0 && !indices.is_empty() {
            return Err(cache_full());
        }
        let rows = indices
            .into_iter()
            .take(capacity)
            .map(|i| {
                self.rows.get(i as usize).ok_or_else(|| {
                    fx_error_to_napi(FxError::InvalidInput("invalid listing offset".into()))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut additions = Vec::new();
        let mut outcome = ReconcileOutcome::default();
        for &(path, row) in &rows {
            if let Some(existing) = store.get_by_path(path) {
                if existing.kind == row.kind {
                    if store
                        .update(existing.id, row.clone())
                        .map_err(fx_error_to_napi)?
                    {
                        outcome.changed_ids.insert(existing.id);
                    }
                    continue;
                }
                outcome
                    .changed_ids
                    .extend(store.remove_subtree(existing.id).into_iter().map(|e| e.id));
            }
            additions.push((path.clone(), row.clone()));
        }
        outcome
            .changed_ids
            .extend(store.insert_batch(additions).map_err(fx_error_to_napi)?);
        let retained: Vec<_> = rows
            .iter()
            .filter_map(|(p, _)| store.get_by_path(p))
            .collect();
        let ids: Vec<_> = retained.iter().map(|e| e.id).collect();
        store.trim_cache(Some(self.parent), &ids);
        outcome.child_set_changed.insert(self.parent);
        self.explorer.emit_reconcile_notice(self.version, &outcome);
        Ok(Some(
            retained
                .iter()
                .map(|entry| EntryJs::from_core(entry))
                .collect(),
        ))
    }
}
