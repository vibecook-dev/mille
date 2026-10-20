//! Filename/path fuzzy matcher wrapping nucleo.
//!
//! Phase 10 scope: a simple one-shot `search()` against a `StoreSnapshot` —
//! no incremental scoring, no live reindexing. Phase 12 benchmarks may
//! justify an incremental matcher, but nucleo's own benches put a
//! ~10k-entry corpus in the 1-2 ms range which is fine for the v0.1
//! synchronous API.
//!
//! Content search (grep-in-files) is NOT this; SPEC §1 keeps it as a
//! separate downstream package.

use crate::entry::{EntryId, EntryKind};
use crate::snapshot::StoreSnapshot;

/// Per-hit score. Higher is better. Nucleo emits `u32` under the hood;
/// we widen to `i32` so the NAPI boundary can expose it as a plain JS
/// `number` without any bigint dance. Scores are NOT normalized — the
/// absolute value is only meaningful relative to other hits in the same
/// result set.
pub type SearchScore = i32;

/// One fuzzy-match result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: EntryId,
    pub score: SearchScore,
    /// Char indices (0-based) into the matched entry's `name` where the
    /// needle landed. Useful for highlighting in UI. Always sorted and
    /// deduped so the UI can walk them linearly.
    pub matched_indices: Vec<u32>,
}

/// Options for a `search` call.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// Cap on the number of returned hits. `None` means unbounded — use
    /// sparingly; defaults to `Some(100)` which matches api.d.ts.
    pub limit: Option<usize>,
    /// When true, include entries marked `is_ignored` or `is_hidden`.
    /// Defaults to false.
    pub include_ignored: bool,
    /// When true, the matcher respects case. When false, uses nucleo's
    /// "smart case" — lowercase needle matches case-insensitively, any
    /// uppercase character triggers case-sensitive match.
    pub case_sensitive: bool,
    /// Only entries of these kinds can match; `None` admits every kind and
    /// an empty list admits none. Applied before scoring and before
    /// `limit`, so a directories-only search with `limit: Some(100)` returns
    /// up to 100 directories — not the directories among the first 100
    /// hits, which a caller filtering afterwards would get.
    pub kinds: Option<Vec<EntryKind>>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: Some(100),
            include_ignored: false,
            case_sensitive: false,
            kinds: None,
        }
    }
}

/// Fuzzy-match `query` against every entry's `name` in `snap` and return
/// a score-descending `Vec<SearchHit>`. Ties break by ascending `EntryId`
/// so output is deterministic.
///
/// Empty query returns an empty vec — we don't want to leak a "rank 0,
/// match everything" corpus through to UI on a stray keystroke.
pub fn search(snap: &StoreSnapshot, query: &str, options: &SearchOptions) -> Vec<SearchHit> {
    use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo::{Config, Matcher, Utf32String};

    if query.is_empty() {
        return Vec::new();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let case = if options.case_sensitive {
        CaseMatching::Respect
    } else {
        CaseMatching::Smart
    };
    let pattern = Pattern::new(query, case, Normalization::Smart, AtomKind::Fuzzy);

    let mut hits: Vec<SearchHit> = Vec::new();
    let mut indices_buf: Vec<u32> = Vec::new();

    for (id, entry) in snap.entries_iter() {
        if !options.include_ignored && (entry.is_ignored_or_excluded() || entry.is_hidden) {
            continue;
        }
        if let Some(kinds) = &options.kinds {
            if !kinds.contains(&entry.kind) {
                continue;
            }
        }
        let haystack = Utf32String::from(entry.name.as_str());
        indices_buf.clear();
        let score = pattern.indices(haystack.slice(..), &mut matcher, &mut indices_buf);
        if let Some(score) = score {
            // Pattern::indices can append indices from multiple atoms
            // without sorting/deduping them; flatten for UI use.
            indices_buf.sort_unstable();
            indices_buf.dedup();
            hits.push(SearchHit {
                id,
                score: score as i32,
                matched_indices: indices_buf.clone(),
            });
        }
    }

    // Descending by score; tie-break by ascending EntryId for determinism.
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));

    if let Some(limit) = options.limit {
        hits.truncate(limit);
    }

    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryKind};
    use std::sync::Arc;

    fn mk_entry(id: EntryId, name: &str) -> Entry {
        Entry {
            id,
            parent_id: None,
            name: name.into(),
            kind: EntryKind::File,
            size: 0,
            mtime_ms: 0,
            ctime_ms: 0,
            symlink_target_is_dir: None,
            path_segments: None,
            is_ignored: false,
            is_excluded: false,
            is_readonly: false,
            is_hidden: false,
        }
    }

    /// Seed a flat list of files into a snapshot (no parent/children
    /// scaffolding — search only reads entries_iter, so keep the fixture
    /// minimal).
    fn seed_names(names: &[&str]) -> StoreSnapshot {
        let mut snap = StoreSnapshot::empty();
        for (i, name) in names.iter().enumerate() {
            let id = EntryId((i as u64) + 1);
            let e = mk_entry(id, name);
            snap.entries.insert(id, Arc::new(e));
        }
        snap
    }

    fn seed_entries(entries: Vec<Entry>) -> StoreSnapshot {
        let mut snap = StoreSnapshot::empty();
        for e in entries {
            let id = e.id;
            snap.entries.insert(id, Arc::new(e));
        }
        snap
    }

    #[test]
    fn empty_query_returns_empty() {
        let snap = seed_names(&["main.rs", "lib.rs"]);
        let hits = search(&snap, "", &SearchOptions::default());
        assert!(hits.is_empty());
    }

    #[test]
    fn exact_filename_match_scores_non_zero() {
        let snap = seed_names(&["main.rs", "lib.rs", "other.txt"]);
        let hits = search(&snap, "main", &SearchOptions::default());
        assert!(!hits.is_empty());
        // main.rs should be the top hit.
        let top = &hits[0];
        assert_eq!(top.id, EntryId(1));
        assert!(top.score > 0);
        // Matched indices should cover 'm','a','i','n' (chars 0..=3).
        assert_eq!(top.matched_indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn fuzzy_skip_chars_matches_with_lower_score_than_exact() {
        // "mn" should fuzzy-match "main" (m..n skipping 'ai') in main.rs.
        let snap = seed_names(&["main.rs", "main", "other.txt"]);
        let fuzzy = search(&snap, "mn", &SearchOptions::default());
        let exact = search(&snap, "main", &SearchOptions::default());
        assert!(!fuzzy.is_empty(), "fuzzy skip-match should still hit");
        assert!(!exact.is_empty());
        // Top exact score should beat the top fuzzy score when the needle
        // is an actual substring of the haystack.
        assert!(
            exact[0].score > fuzzy[0].score,
            "exact {} should beat fuzzy {}",
            exact[0].score,
            fuzzy[0].score
        );
    }

    #[test]
    fn non_matching_query_returns_empty() {
        let snap = seed_names(&["main.rs", "lib.rs"]);
        let hits = search(&snap, "zzzzz", &SearchOptions::default());
        assert!(hits.is_empty());
    }

    #[test]
    fn ignored_entries_excluded_unless_include_ignored() {
        let mut e1 = mk_entry(EntryId(1), "main.rs");
        let mut e2 = mk_entry(EntryId(2), "main.rs.bak");
        e2.is_ignored = true;
        let _ = &mut e1;
        let snap = seed_entries(vec![e1, e2]);

        let hits = search(&snap, "main", &SearchOptions::default());
        let ids: Vec<EntryId> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![EntryId(1)]);

        let hits_all = search(
            &snap,
            "main",
            &SearchOptions {
                include_ignored: true,
                ..SearchOptions::default()
            },
        );
        let ids_all: Vec<EntryId> = hits_all.iter().map(|h| h.id).collect();
        assert_eq!(ids_all.len(), 2);
        assert!(ids_all.contains(&EntryId(1)));
        assert!(ids_all.contains(&EntryId(2)));
    }

    #[test]
    fn hidden_entries_excluded_unless_include_ignored() {
        let mut e1 = mk_entry(EntryId(1), "main.rs");
        let mut e2 = mk_entry(EntryId(2), ".main.rs.swp");
        e2.is_hidden = true;
        let _ = &mut e1;
        let snap = seed_entries(vec![e1, e2]);

        let hits = search(&snap, "main", &SearchOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, EntryId(1));
    }

    #[test]
    fn limit_truncates_results() {
        let names: Vec<String> = (0..10).map(|i| format!("main_{:02}.rs", i)).collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let snap = seed_names(&name_refs);

        let opts = SearchOptions {
            limit: Some(3),
            ..SearchOptions::default()
        };
        let hits = search(&snap, "main", &opts);
        assert_eq!(hits.len(), 3);

        let opts_unbounded = SearchOptions {
            limit: None,
            ..SearchOptions::default()
        };
        let hits_all = search(&snap, "main", &opts_unbounded);
        assert_eq!(hits_all.len(), 10);
    }

    #[test]
    fn kinds_filter_applies_before_the_limit() {
        let mut entries: Vec<Entry> = (0..8)
            .map(|i| mk_entry(EntryId(i + 1), &format!("src_file_{i}.rs")))
            .collect();
        for (i, name) in ["src", "src_tools"].iter().enumerate() {
            let mut dir = mk_entry(EntryId(100 + i as u64), name);
            dir.kind = EntryKind::Directory;
            entries.push(dir);
        }
        let snap = seed_entries(entries);

        let dirs_only = SearchOptions {
            limit: Some(1),
            kinds: Some(vec![EntryKind::Directory]),
            ..SearchOptions::default()
        };
        let hits = search(&snap, "src", &dirs_only);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].id.0 >= 100,
            "a one-hit directory search returns a directory"
        );

        let all_dirs = SearchOptions {
            limit: Some(100),
            kinds: Some(vec![EntryKind::Directory]),
            ..SearchOptions::default()
        };
        let mut ids: Vec<u64> = search(&snap, "src", &all_dirs)
            .iter()
            .map(|h| h.id.0)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![100, 101]);

        let none = SearchOptions {
            kinds: Some(Vec::new()),
            ..SearchOptions::default()
        };
        assert!(search(&snap, "src", &none).is_empty());
        assert_eq!(search(&snap, "src", &SearchOptions::default()).len(), 10);
    }

    #[test]
    fn case_insensitive_by_default_via_smart_case() {
        let snap = seed_names(&["Main.rs", "LIB.rs", "other.txt"]);
        // All-lowercase needle should match the uppercase haystack names.
        let hits = search(&snap, "main", &SearchOptions::default());
        let ids: Vec<EntryId> = hits.iter().map(|h| h.id).collect();
        assert!(ids.contains(&EntryId(1)));
    }

    #[test]
    fn case_sensitive_mode_respects_case() {
        let snap = seed_names(&["Main.rs", "main.rs"]);
        // Under Respect, lowercase 'main' should NOT match uppercase 'Main'.
        let opts = SearchOptions {
            case_sensitive: true,
            ..SearchOptions::default()
        };
        let hits = search(&snap, "main", &opts);
        let ids: Vec<EntryId> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![EntryId(2)]);
    }

    // ─── Commit 10.3: correctness + perf sanity ──────────────────────────

    #[test]
    fn scores_are_monotonically_non_increasing() {
        let names: Vec<String> = (0..20).map(|i| format!("main_{}_file.rs", i)).collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let snap = seed_names(&name_refs);
        let hits = search(&snap, "mn", &SearchOptions::default());
        for w in hits.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "scores should be non-increasing: {} -> {}",
                w[0].score,
                w[1].score
            );
        }
    }

    #[test]
    fn output_is_deterministic_across_runs() {
        let snap = seed_names(&["main.rs", "lib.rs", "mod.rs", "Manifest.toml"]);
        let opts = SearchOptions::default();
        let a = search(&snap, "m", &opts);
        let b = search(&snap, "m", &opts);
        let c = search(&snap, "m", &opts);
        let ids_a: Vec<EntryId> = a.iter().map(|h| h.id).collect();
        let ids_b: Vec<EntryId> = b.iter().map(|h| h.id).collect();
        let ids_c: Vec<EntryId> = c.iter().map(|h| h.id).collect();
        assert_eq!(ids_a, ids_b);
        assert_eq!(ids_b, ids_c);
    }

    #[test]
    fn exact_beats_fuzzy_when_both_present() {
        // Mixed corpus: one haystack contains the exact substring, others
        // only satisfy the fuzzy (skip-char) match.
        let snap = seed_names(&[
            "maaaain.rs",    // fuzzy-only: needle 'main' skips 'aaaa'
            "main_exact.rs", // exact substring at pos 0
            "mxaxixn.rs",    // fuzzy-only
        ]);
        let hits = search(&snap, "main", &SearchOptions::default());
        assert!(!hits.is_empty());
        // The exact-substring candidate should come out on top.
        assert_eq!(hits[0].id, EntryId(2));
    }

    #[test]
    fn large_tree_completes_under_budget() {
        // 500 entries — this is a smoke-test for the Phase 10.3 correctness
        // commit, not a real bench. Debug-build criterion lives in mille-bench.
        let names: Vec<String> = (0..500)
            .map(|i| format!("file_{:04}_thing_{}.rs", i, i % 17))
            .collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let snap = seed_names(&name_refs);

        let start = std::time::Instant::now();
        let hits = search(&snap, "thing", &SearchOptions::default());
        let elapsed = start.elapsed();
        assert!(!hits.is_empty());
        // Debug build should still come in well under 10ms.
        assert!(
            elapsed.as_millis() < 50,
            "500-entry search took {:?}",
            elapsed
        );
    }
}
