use std::{fs, io::Write, path::Path};

use crate::Result;

/// Points inside [`publish`] where a test can abort the sequence.
///
/// A crash can only leave the target slot as its previous content, absent, or
/// the new content. These names make each of those states reachable from a test
/// without touching the production path.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailPoint {
    BeforeRemove,
    AfterRemove,
    AfterRename,
}

/// Armed failures, scoped to one project root each.
///
/// `publish` runs on a blocking worker thread, so this cannot be thread-local.
/// Scoping by root and keeping a list keeps parallel tests from clobbering or
/// consuming each other's failure.
#[cfg(test)]
static FAIL_AT: std::sync::Mutex<Vec<(std::path::PathBuf, FailPoint)>> =
    std::sync::Mutex::new(Vec::new());

/// Arms a one-shot failure at `point` for publishes under `root`.
#[cfg(test)]
pub(crate) fn fail_at(root: &Path, point: FailPoint) {
    FAIL_AT
        .lock()
        .expect("fail point lock")
        .push((root.to_path_buf(), point));
}

#[cfg(test)]
fn trip(target: &Path, point: FailPoint) -> Result<()> {
    let mut armed = FAIL_AT.lock().expect("fail point lock");
    let index = armed
        .iter()
        .position(|(root, at)| *at == point && target.starts_with(root));
    let Some(index) = index else {
        return Ok(());
    };
    armed.swap_remove(index);
    Err(crate::Error::invalid("injected publish failure"))
}

pub(crate) fn publish(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| crate::Error::invalid("published path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;

    // State slots alternate, so removing the inactive target cannot destroy the
    // currently committed state. The final rename is the publication point.
    #[cfg(test)]
    trip(target, FailPoint::BeforeRemove)?;
    if target.try_exists()? {
        fs::remove_file(target)?;
    }
    #[cfg(test)]
    trip(target, FailPoint::AfterRemove)?;
    file.persist(target).map_err(|error| error.error)?;
    #[cfg(test)]
    trip(target, FailPoint::AfterRename)?;
    sync_parent(target)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}
