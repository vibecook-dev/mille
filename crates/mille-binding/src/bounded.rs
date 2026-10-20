//! Small bounded-read helpers shared by the explorer and the watch runtime
//! (petition M10). Each replaces a direct `std::fs` probe that could block in
//! the kernel while the caller held `policy_gate`.

use std::path::Path;

use mille_core::{ErrorCode, FxError, IgnoreMatcher, ReadBoundary, ReadFailure};

/// Stack the ignore files directly inside `dir` onto every matcher given,
/// reading them through the bounded pool. Replaces the `is_file()` +
/// `add_from_file` pairs: a lookup inside an evicted folder materializes it,
/// so even "is there a .gitignore here" must not run on the engine thread.
pub(crate) fn seed_ignore_files(
    reads: &ReadBoundary,
    dir: &Path,
    matchers: &mut [&mut IgnoreMatcher],
) -> Result<(), FxError> {
    let files = reads
        .ignore_files(dir)
        .map_err(|failure| failure.into_fx(dir))?;
    for file in &files {
        for matcher in matchers.iter_mut() {
            // Best effort, as `add_from_file` callers always were.
            let _ = matcher.add_from_contents(&file.path, &file.contents);
        }
    }
    Ok(())
}

/// `Path::exists`, bounded: an OS error still reads as "missing", but a stat
/// that did not answer is an error, never evidence that the path is gone.
pub(crate) fn probe_exists(reads: &ReadBoundary, path: &Path) -> Result<bool, FxError> {
    match reads.stat(path, false) {
        Ok(output) => Ok(output
            .as_stat()
            .is_some_and(|stat| !stat.metadata.file_type().is_symlink() || stat.target.is_some())),
        Err(ReadFailure::Io(_)) => Ok(false),
        Err(failure) => Err(failure.into_fx(path)),
    }
}

/// `Path::is_dir`, bounded, with the same rule as `probe_exists`.
pub(crate) fn probe_is_dir(reads: &ReadBoundary, path: &Path) -> Result<bool, FxError> {
    match reads.stat(path, false) {
        Ok(output) => Ok(output.as_stat().is_some_and(|stat| stat.is_dir_like())),
        Err(ReadFailure::Io(_)) => Ok(false),
        Err(failure) => Err(failure.into_fx(path)),
    }
}

/// The read did not answer — it timed out, is stuck, or the folder is not
/// downloaded. Says nothing about whether the path exists.
pub(crate) fn is_unanswered(error: &FxError) -> bool {
    matches!(error.code(), ErrorCode::ETIMEDOUT | ErrorCode::EDATALESS)
}
