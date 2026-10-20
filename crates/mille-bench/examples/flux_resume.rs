//! flux dogfood measurements for the resume paths (not a criterion bench:
//! it needs a real library and, for `catchup`, a scratch copy it may
//! change).
//!
//! ```text
//! flux_resume snapshot <root> <scratch-dir> [runs]
//!     The resume snapshot of a flux-shaped media walk, written through an
//!     EntryStore (populate_store + write_snapshot) and without one
//!     (write_snapshot_from).
//! flux_resume catchup <scratch-root> [runs] [n,n,...]
//!     For each n: capture a history token, create n files spread over the
//!     tree, then time the FSEvents replay + the per-directory rescans it
//!     asks for, against a full walk and against events_since. The files
//!     are removed again after each run.
//! ```
//!
//! One JSON object per line on stdout.

#[cfg(unix)]
mod imp {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use mille_core::{
        events_since, populate_store, read_snapshot, replay_since, walk_scan, write_snapshot,
        write_snapshot_from, write_snapshot_from_with, Durability, EntryKind, EntryStore,
        FileFilter, HistoryToken, ReadBoundary, ReadBoundaryOptions, Replay, ReplayOptions,
        RootStat, ScanOptions, Visit, WalkedEntry,
    };

    const EXTENSIONS: &[&str] = &[
        "mkv", "mk3d", "webm", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "mts", "mpg", "mpeg",
        "vob", "wmv", "flv", "ogv", "iso", "srt", "ass", "ssa", "vtt", "sub", "idx", "sup", "nfo",
        "jpg", "jpeg", "png", "webp",
    ];

    fn options(parallelism: usize) -> ScanOptions {
        ScanOptions {
            parallelism,
            file_filter: Some(FileFilter::extensions(EXTENSIONS)),
            ..ScanOptions::media()
        }
    }

    fn reads() -> ReadBoundary {
        ReadBoundary::new(ReadBoundaryOptions {
            concurrency: 16,
            ..ReadBoundaryOptions::media()
        })
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }

    fn median(v: &mut [f64]) -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() {
            return f64::NAN;
        }
        v[v.len() / 2]
    }

    fn load() -> String {
        let mut avg = [0f64; 3];
        // SAFETY: getloadavg writes at most 3 doubles.
        let n = unsafe { libc_getloadavg(avg.as_mut_ptr(), 3) };
        if n == 3 {
            format!("[{:.2},{:.2},{:.2}]", avg[0], avg[1], avg[2])
        } else {
            "null".into()
        }
    }

    extern "C" {
        #[link_name = "getloadavg"]
        fn libc_getloadavg(loadavg: *mut f64, nelem: i32) -> i32;
    }

    fn mtime_ms(ns: i64) -> i64 {
        ns.div_euclid(1_000_000)
    }

    /// The walk flux keeps for its resume snapshot (`flux-scan/src/resume.rs`):
    /// one `WalkedEntry` per delivered entry, the root not included.
    fn flux_walk(root: &Path, reads: &ReadBoundary) -> (Vec<WalkedEntry>, Vec<PathBuf>, Duration) {
        let t = Instant::now();
        let mut entries = Vec::new();
        let mut dirs = Vec::new();
        let report = walk_scan(root, &options(16), reads, |dir, e| {
            let kind = if e.is_symlink {
                EntryKind::Symlink
            } else {
                e.kind
            };
            let path = dir.join(e.name);
            if e.is_dir() && !e.is_symlink {
                dirs.push(path.clone());
            }
            entries.push(WalkedEntry {
                path,
                parent_path: Some(dir.path.to_path_buf()),
                name: e.name.to_string_lossy().into_owned(),
                kind,
                size: if kind == EntryKind::File { e.size } else { 0 },
                mtime_ms: mtime_ms(e.mtime_ns),
                ctime_ms: mtime_ms(e.mtime_ns),
                is_symlink: e.is_symlink,
                symlink_target_is_dir: e.is_symlink.then(|| e.is_dir()),
                is_readonly: false,
                is_hidden: false,
                depth: (dir.depth + 1) as u16,
            });
            Visit::Continue
        })
        .expect("walk");
        assert!(report.is_complete(), "{:?}", report.unread);
        (entries, dirs, t.elapsed())
    }

    fn root_stats(root: &Path) -> Vec<RootStat> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(root).unwrap();
        vec![RootStat {
            path: root.to_path_buf(),
            mtime_ms: m.mtime() * 1000 + m.mtime_nsec() / 1_000_000,
        }]
    }

    fn snapshot(root: &Path, scratch: &Path, runs: usize) {
        let reads = reads();
        let (walked, _, walk) = flux_walk(root, &reads);
        let stats = root_stats(root);
        let (mut store_ms, mut from_ms, mut unsynced_ms, mut populate_ms) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (a, b, c) = (
            scratch.join("store.bin"),
            scratch.join("from.bin"),
            scratch.join("unsynced.bin"),
        );
        for _ in 0..runs {
            let t = Instant::now();
            let store = EntryStore::new();
            let mut with_root = Vec::with_capacity(walked.len() + 1);
            with_root.push(WalkedEntry {
                path: root.to_path_buf(),
                parent_path: None,
                name: root.file_name().unwrap().to_string_lossy().into_owned(),
                kind: EntryKind::Directory,
                size: 0,
                mtime_ms: stats[0].mtime_ms,
                ctime_ms: stats[0].mtime_ms,
                is_symlink: false,
                symlink_target_is_dir: None,
                is_readonly: false,
                is_hidden: false,
                depth: 0,
            });
            with_root.extend(walked.iter().cloned());
            let p = Instant::now();
            let ids = populate_store(&store, root, &with_root, None).unwrap();
            populate_ms.push(ms(p.elapsed()));
            let next = ids.iter().map(|i| i.raw()).max().unwrap_or(0) + 1;
            write_snapshot(&store.snapshot(), next, &stats, &a).unwrap();
            store_ms.push(ms(t.elapsed()));

            let t = Instant::now();
            write_snapshot_from(&walked, &stats, &b).unwrap();
            from_ms.push(ms(t.elapsed()));

            let t = Instant::now();
            write_snapshot_from_with(&walked, &stats, &c, Durability::Unsynced).unwrap();
            unsynced_ms.push(ms(t.elapsed()));
        }
        // The I/O floor: the same bytes, written, fsynced, renamed, parent
        // fsynced, as write_snapshot does.
        let bytes = std::fs::read(&b).unwrap();
        let mut io_ms = Vec::new();
        let mut nosync_ms = Vec::new();
        for _ in 0..runs {
            use std::io::Write;
            let tmp = scratch.join("io.tmp");
            let t = Instant::now();
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(&bytes).unwrap();
            f.sync_all().unwrap();
            std::fs::rename(&tmp, scratch.join("io.bin")).unwrap();
            std::fs::File::open(scratch).unwrap().sync_all().unwrap();
            io_ms.push(ms(t.elapsed()));
            let t = Instant::now();
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(&bytes).unwrap();
            drop(f);
            std::fs::rename(&tmp, scratch.join("io.bin")).unwrap();
            nosync_ms.push(ms(t.elapsed()));
        }
        let _ = std::fs::remove_file(scratch.join("io.bin"));
        println!(
            "{{\"bench\":\"snapshot_io_floor\",\"bytes\":{},\"write_fsync_rename_ms\":{:.1},\"write_rename_no_fsync_ms\":{:.1},\"all_fsync\":{:?},\"all_nosync\":{:?}}}",
            bytes.len(),
            median(&mut io_ms.clone()),
            median(&mut nosync_ms.clone()),
            round(&io_ms),
            round(&nosync_ms)
        );
        let same = {
            let (x, y) = (read_snapshot(&a).unwrap(), read_snapshot(&b).unwrap());
            x.entries == y.entries && x.children == y.children && x.roots == y.roots
        };
        let size = std::fs::metadata(&b).map(|m| m.len()).unwrap_or(0);
        println!(
            "{{\"bench\":\"snapshot\",\"root\":{:?},\"entries\":{},\"walk_ms\":{:.1},\"store_path_ms\":{:.1},\"populate_store_only_ms\":{:.1},\"write_snapshot_from_ms\":{:.1},\"write_snapshot_from_unsynced_ms\":{:.1},\"store_path_all\":{:?},\"from_all\":{:?},\"unsynced_all\":{:?},\"same_entries\":{},\"bytes\":{},\"load\":{}}}",
            root,
            walked.len(),
            ms(walk),
            median(&mut store_ms.clone()),
            median(&mut populate_ms.clone()),
            median(&mut from_ms.clone()),
            median(&mut unsynced_ms.clone()),
            store_ms.iter().map(|v| (v * 10.0).round() / 10.0).collect::<Vec<_>>(),
            from_ms.iter().map(|v| (v * 10.0).round() / 10.0).collect::<Vec<_>>(),
            round(&unsynced_ms),
            same,
            size,
            load()
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        let _ = std::fs::remove_file(&c);
    }

    fn catchup(root: &Path, runs: usize, ns: &[usize]) {
        let reads = reads();
        let (walked, dirs, _) = flux_walk(root, &reads);
        let stats = root_stats(root);
        let scratch = root.with_extension("snap");
        std::fs::create_dir_all(&scratch).unwrap();
        let snap_path = scratch.join("resume.bin");
        for &n in ns {
            let (mut replay_ms, mut rescan_ms, mut full_ms, mut since_ms) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let (mut records, mut dirs_rescanned, mut found) = (0u64, 0usize, 0usize);
            for run in 0..runs {
                // events_since needs a snapshot of the tree as it is now.
                write_snapshot_from(&walked, &stats, &snap_path).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                let token = HistoryToken::capture(root).unwrap();
                let created: Vec<PathBuf> = (0..n)
                    .map(|i| {
                        let dir = &dirs[(i * 7919 + run * 104_729) % dirs.len()];
                        let p = dir.join(format!("phase-d-{run}-{i}.mkv"));
                        std::fs::write(&p, b"phase d").unwrap();
                        p
                    })
                    .collect();
                // Let fseventsd number them.
                std::thread::sleep(Duration::from_millis(300));

                let t = Instant::now();
                let report = replay_since(&token, &ReplayOptions::default()).unwrap();
                replay_ms.push(ms(t.elapsed()));
                let Replay::Changes(changes) = report.replay else {
                    panic!("replay asked for a walk: {:?}", report.replay)
                };
                records = changes.records;
                found = created
                    .iter()
                    .filter(|p| changes.changed.iter().any(|e| &e.path == *p))
                    .count();
                let rescan = changes.dirs_to_rescan(root);
                dirs_rescanned = rescan.len();
                let t = Instant::now();
                let mut seen = 0usize;
                for dir in &rescan {
                    let one = ScanOptions {
                        max_depth: Some(1),
                        ..options(16)
                    };
                    let _ = walk_scan(dir, &one, &reads, |_, _| {
                        seen += 1;
                        Visit::Continue
                    })
                    .unwrap();
                }
                rescan_ms.push(ms(t.elapsed()));

                let t = Instant::now();
                let mut all = 0usize;
                let _ = walk_scan(root, &options(16), &reads, |_, _| {
                    all += 1;
                    Visit::Continue
                })
                .unwrap();
                full_ms.push(ms(t.elapsed()));

                let snap = read_snapshot(&snap_path).unwrap();
                let t = Instant::now();
                let ev = events_since(&snap).unwrap();
                since_ms.push(ms(t.elapsed()));
                assert!(ev.len() >= n, "events_since saw {} of {n}", ev.len());
                assert!(seen >= n && all > seen);

                for p in &created {
                    std::fs::remove_file(p).unwrap();
                }
            }
            let catchup: Vec<f64> = replay_ms
                .iter()
                .zip(&rescan_ms)
                .map(|(a, b)| a + b)
                .collect();
            println!(
                "{{\"bench\":\"catchup\",\"n\":{n},\"found\":{found},\"records\":{records},\"dirs_rescanned\":{dirs_rescanned},\"replay_ms\":{:.1},\"rescan_dirs_ms\":{:.1},\"catchup_ms\":{:.1},\"full_walk_ms\":{:.1},\"events_since_ms\":{:.1},\"all\":{{\"replay\":{:?},\"rescan\":{:?},\"full\":{:?},\"events_since\":{:?}}},\"load\":{}}}",
                median(&mut replay_ms.clone()),
                median(&mut rescan_ms.clone()),
                median(&mut catchup.clone()),
                median(&mut full_ms.clone()),
                median(&mut since_ms.clone()),
                round(&replay_ms),
                round(&rescan_ms),
                round(&full_ms),
                round(&since_ms),
                load()
            );
        }
        let _ = std::fs::remove_file(&snap_path);
        let _ = std::fs::remove_dir(&scratch);
    }

    /// How the replay's cost grows with the *volume's* activity since the
    /// token: the same root, tokens moved back by `window` event ids.
    fn window(root: &Path, runs: usize) {
        for window in [10_000u64, 100_000, 1_000_000, 10_000_000] {
            let mut took = Vec::new();
            let mut outcome = String::new();
            for _ in 0..runs {
                let now = HistoryToken::capture(root).unwrap();
                let token = HistoryToken {
                    event_id: now.event_id.saturating_sub(window),
                    ..now
                };
                let report = replay_since(&token, &ReplayOptions::default()).unwrap();
                took.push(ms(report.elapsed));
                outcome = match report.replay {
                    Replay::Changes(c) => format!("changes, {} records", c.records),
                    Replay::Rescan(r) => format!("rescan {r:?}"),
                };
            }
            println!(
                "{{\"bench\":\"replay_window\",\"event_ids_back\":{window},\"replay_ms\":{:.1},\"all\":{:?},\"outcome\":{:?},\"load\":{}}}",
                median(&mut took.clone()),
                round(&took),
                outcome,
                load()
            );
        }
    }

    fn round(v: &[f64]) -> Vec<f64> {
        v.iter().map(|x| (x * 10.0).round() / 10.0).collect()
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        let runs = |i: usize| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(5);
        match args.get(1).map(String::as_str) {
            Some("snapshot") if args.len() >= 4 => {
                snapshot(Path::new(&args[2]), Path::new(&args[3]), runs(4))
            }
            Some("window") if args.len() >= 3 => window(Path::new(&args[2]), runs(3)),
            Some("catchup") if args.len() >= 3 => {
                let ns: Vec<usize> = args
                    .get(4)
                    .map(|s| s.split(',').filter_map(|n| n.parse().ok()).collect())
                    .unwrap_or_else(|| vec![0, 1, 10, 100, 1000]);
                catchup(Path::new(&args[2]), runs(3), &ns)
            }
            _ => {
                eprintln!(
                    "usage: flux_resume snapshot <root> <scratch-dir> [runs]\n       flux_resume catchup <scratch-root> [runs] [n,n,...]\n       flux_resume window <root> [runs]"
                );
                std::process::exit(2);
            }
        }
    }
}

fn main() {
    #[cfg(unix)]
    imp::main();
    #[cfg(not(unix))]
    eprintln!("flux_resume: unix only");
}
