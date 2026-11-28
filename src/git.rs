use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::path_kind;
use crate::Language;

/// True if the commit's raw object records a parent, even when libgit2 hides it
/// (shallow boundary). A true root has no `parent ` header line.
fn has_raw_parent(repo: &git2::Repository, commit: &git2::Commit) -> Result<bool> {
    let odb = repo.odb()?;
    let obj = odb.read(commit.id())?;
    let header = obj
        .data()
        .split(|&b| b == b'\n')
        .take_while(|l| !l.is_empty());
    Ok(header.into_iter().any(|l| l.starts_with(b"parent ")))
}

#[non_exhaustive]
pub struct GitRepo {
    root: PathBuf,
    /// Canonicalized `root`, computed once; used to validate file reads.
    canonical_root: PathBuf,
}

/// Marker payload of the `io::Error` returned when a read is refused because
/// the resolved target is outside the repo, inside a `.git` directory, or an
/// untracked file (symlink target, or a path absent from the git index).
#[derive(Debug)]
pub(crate) struct PathEscape;

impl std::fmt::Display for PathEscape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("path resolves outside the repository, into .git, or to an untracked file")
    }
}

impl std::error::Error for PathEscape {}

/// True if `e` is a [`PathEscape`] refusal (as opposed to a genuine I/O error
/// such as EACCES).
pub(crate) fn is_path_escape(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<PathEscape>())
}

/// Read `path` as UTF-8 text only if its fully resolved target lies inside
/// `canonical_root` and has no `.git` path component at any depth (#202).
/// Refusals carry a [`PathEscape`] payload. Symlinks, `..` and symlinked
/// directories are all resolved by canonicalize immediately before the read.
pub(crate) fn read_within_root(canonical_root: &Path, path: &Path) -> std::io::Result<String> {
    let bytes = read_bytes_within_root(canonical_root, path)?;
    String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// True if `rel` has an entry at any stage (0 = normal, 1-3 = unresolved merge
/// conflict) in `idx`. A conflicted file has no stage-0 entry but is still
/// tracked by git, so it must count; a file removed with `git rm --cached` has
/// no entry at any stage and does not (#563, #395).
pub(crate) fn in_git_index(idx: &git2::Index, rel: &Path) -> bool {
    (0..=3).any(|stage| idx.get_path(rel, stage).is_some())
}

/// True if `real` (canonical, inside `canonical_root`) is an entry (any stage,
/// see [`in_git_index`]) in the repo's index. Fails closed on any error.
fn is_tracked_in_index(
    canonical_root: &Path,
    real: &Path,
    index: &std::cell::OnceCell<Option<git2::Index>>,
) -> bool {
    let Ok(rel) = real.strip_prefix(canonical_root) else {
        return false;
    };
    let Some(rel) = rel.to_str() else {
        return false;
    };
    // Opened at most once per cell; an open/read error is cached as `None`
    // (fail closed: nothing counts as tracked).
    index
        .get_or_init(|| {
            git2::Repository::open(canonical_root)
                .and_then(|r| r.index())
                .ok()
        })
        .as_ref()
        .is_some_and(|idx| in_git_index(idx, Path::new(rel)))
}

/// True if `path` or any ancestor below the repo root is a symlink. Walks up,
/// checking each component itself *before* deciding whether it is the root, so a
/// link that resolves to the root (`docs -> .`) is still caught. Fails closed
/// (true) if the root is never reached.
fn path_has_symlink_component(canonical_root: &Path, path: &Path) -> bool {
    for (i, anc) in path.ancestors().enumerate() {
        match std::fs::symlink_metadata(anc) {
            Ok(m) if m.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
        if i > 0 && anc.canonicalize().is_ok_and(|p| p == canonical_root) {
            return false;
        }
    }
    true
}

/// Largest file (bytes) that is read and indexed; bigger files are refused with
/// `ErrorKind::FileTooLarge` before any content is read (#567). Provisional.
pub(crate) const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) fn read_bytes_within_root(
    canonical_root: &Path,
    path: &Path,
) -> std::io::Result<Vec<u8>> {
    read_bytes_within_root_with(
        canonical_root,
        path,
        &std::cell::OnceCell::new(),
        MAX_FILE_BYTES,
    )
}

/// Like [`read_bytes_within_root`], but reuses a lazily opened git index across
/// calls (one open per scan instead of one per tracked symlink).
fn read_bytes_within_root_with(
    canonical_root: &Path,
    path: &Path,
    index: &std::cell::OnceCell<Option<git2::Index>>,
    max_bytes: u64,
) -> std::io::Result<Vec<u8>> {
    let real = path.canonicalize()?;
    let denied = match real.strip_prefix(canonical_root) {
        Err(_) => true,
        Ok(rest) => rest.components().any(|c| c.as_os_str() == ".git"),
    };
    if denied {
        return Err(std::io::Error::other(PathEscape));
    }
    // A symlink may only lead to a *tracked* file; otherwise a committed link
    // (e.g. x.py -> .env) would expose untracked / gitignored files (#328).
    // Any path component (not just the last) may be a symlink, e.g. a tracked
    // `docs -> ignored_dir` directory link.
    let is_link = path_has_symlink_component(canonical_root, path);
    if is_link && !is_tracked_in_index(canonical_root, &real, index) {
        return Err(std::io::Error::other(PathEscape));
    }
    // `real` is canonical, so it must not itself be a symlink; if it is, it was
    // swapped after the check above (#543). lstat so nothing is followed.
    let meta = std::fs::symlink_metadata(&real)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other(PathEscape));
    }
    // Never open FIFOs / devices / sockets: open(2)/read would block (#494).
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    // Refuse before reading: the whole file would otherwise be held in memory
    // and then expanded into per-symbol rows (#567).
    let too_large = || {
        std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("file is larger than {max_bytes} bytes"),
        )
    };
    if meta.len() > max_bytes {
        return Err(too_large());
    }
    let file = open_verified(&real, &meta)?;
    let mut buf = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    // `take` bounds the read even if the file grows after the size check.
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, max_bytes.saturating_add(1)),
        &mut buf,
    )?;
    if buf.len() as u64 > max_bytes {
        return Err(too_large());
    }
    Ok(buf)
}

/// Opens `real` and confirms the opened file is the one `checked` (an lstat of
/// `real` taken after the containment checks) describes. Closes the window in
/// which a path component could be swapped for a symlink between the check and
/// the open (#543). On Unix the opened fd's dev/ino must equal `checked`'s; on
/// Linux the fd's resolved path (`/proc/self/fd`) must also equal `real`.
/// Elsewhere this is a plain open (no TOCTOU hardening).
fn open_verified(real: &Path, checked: &std::fs::Metadata) -> std::io::Result<std::fs::File> {
    // O_NONBLOCK: if `real` was swapped for a FIFO after `checked` was taken, a
    // plain open waits for a writer forever. A non-blocking open returns at once
    // and the `is_file` check below refuses it. It does not affect regular files.
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(real)?
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(real)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if !opened.is_file() || opened.dev() != checked.dev() || opened.ino() != checked.ino() {
            return Err(std::io::Error::other(PathEscape));
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // Unavailable (no /proc): the dev/ino check above still applies.
        if let Ok(resolved) = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())) {
            if resolved != real {
                return Err(std::io::Error::other(PathEscape));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = checked;
    Ok(file)
}

impl GitRepo {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        path.canonicalize().with_context(|| {
            format!(
                "--repo '{}' does not exist or is not accessible",
                path.display()
            )
        })?;
        let repo = git2::Repository::open(path)
            .with_context(|| format!("'{}' is not a git repository", path.display()))?;
        // Prefer repo.workdir() so the stored root matches git's own logical
        // working directory, avoiding divergence when the path contains symlinks
        // (e.g. network mounts where canonicalize may fail or resolve differently).
        let root = repo
            .workdir()
            .with_context(|| {
                format!(
                    "'{}' is a bare repository (no working directory)",
                    path.display()
                )
            })?
            .to_path_buf();
        let canonical_root = root
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize repo root {:?}", root))?;
        Ok(GitRepo {
            root,
            canonical_root,
        })
    }

    /// Validated read of a tracked file (relative to the repo root). Refuses
    /// targets outside the repo or under `.git`, regardless of file mode.
    pub(crate) fn read_tracked(&self, rel: &Path) -> std::io::Result<String> {
        read_within_root(&self.canonical_root, &self.root.join(rel))
    }

    /// Open the repo's git index once so callers can check many paths with
    /// [`GitRepo::read_indexed`] without reopening it per file.
    pub(crate) fn open_index(&self) -> Result<git2::Index> {
        git2::Repository::open(&self.root)
            .and_then(|r| r.index())
            .with_context(|| format!("Failed to read git index at {:?}", self.root))
    }

    /// Like [`GitRepo::read_tracked`], but additionally refuses a path that is
    /// not an entry (any stage, see [`in_git_index`]) in `index`, so a path taken from an untrusted
    /// index.db cannot expose untracked files such as `.env` (#395). Uncommitted
    /// edits to tracked files are still readable.
    pub(crate) fn read_indexed(&self, index: &git2::Index, rel: &Path) -> std::io::Result<String> {
        if !in_git_index(index, rel) {
            return Err(std::io::Error::other(PathEscape));
        }
        self.read_tracked(rel)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Fail with the "no commits yet" error if HEAD is unborn, before anything is
    /// created. Other `HEAD` failures are left for the scan to report.
    pub fn ensure_has_commits(&self) -> Result<()> {
        match git2::Repository::open(&self.root).and_then(|r| r.head().map(|_| ())) {
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {
                Err(unborn_head_error(&self.root))
            }
            _ => Ok(()),
        }
    }

    /// The canonicalized repository root (symlinks resolved).
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// Unified diff of the last n_commits commits (empty string if the repo has no commits).
    ///
    /// The accumulated patch is capped at `DIFF_MAX_BYTES` (512 KiB). Any content
    /// beyond that limit is dropped and a `[diff truncated]` marker is appended so
    /// callers always receive a valid (possibly incomplete) UTF-8 string that fits
    /// within the token budget.
    ///
    /// When `n_commits` reaches past the root commit the diff is computed
    /// against the empty tree so the initial commit's files are included. In a
    /// shallow clone the oldest reachable commit is not a real root, so the diff
    /// is taken against that commit's tree instead (older history is unavailable).
    /// Non-UTF-8 lines are converted lossily (U+FFFD), not dropped.
    pub fn recent_diff(&self, n_commits: usize) -> Result<String> {
        const DIFF_MAX_BYTES: usize = 512 * 1024; // 512 KiB

        let repo = git2::Repository::open(&self.root)
            .with_context(|| format!("Failed to open git repo at {:?}", self.root))?;
        let head_commit = match repo.head() {
            Ok(h) => h.peel_to_commit()?,
            // No commits yet: there is no history to diff.
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(String::new()),
            Err(e) => return Err(e.into()),
        };

        // 0 means "no diff". Without this, a root-only history would fall into the
        // empty-tree branch below and return the whole initial commit.
        if n_commits == 0 {
            return Ok(String::new());
        }

        // Walk back n commits. If history runs out first (the root commit would
        // need a parent), diff against the empty tree so the root's files count.
        let mut ancestor = head_commit.clone();
        let mut anc_tree_opt = None;
        let mut reached = true;
        for _ in 0..n_commits {
            if ancestor.parent_count() == 0 {
                reached = false;
                break;
            }
            ancestor = ancestor.parent(0)?;
        }
        let head_tree = head_commit.tree()?;
        // A shallow clone's boundary commit has no parent in the object store but
        // is not the true root: diff against its tree (the oldest commit we can
        // see) instead of the empty tree, or the whole repo shows up (#465).
        // `is_shallow()` is repo-wide: a shallow second-parent side must not turn a
        // true first-parent root into a boundary, so inspect the commit itself (#494).
        let at_boundary = !reached && repo.is_shallow() && has_raw_parent(&repo, &ancestor)?;
        if reached || at_boundary {
            anc_tree_opt = Some(ancestor.tree()?);
            if !reached {
                eprintln!(
                    "note: shallow clone: diffing against the oldest available commit {} (older history is unavailable; the diff may be partial or empty)",
                    ancestor.id()
                );
            }
        }
        let diff = repo.diff_tree_to_tree(anc_tree_opt.as_ref(), Some(&head_tree), None)?;

        // Only files still tracked in the index with a known language are
        // shown. A deleted file (e.g. a removed `.env`) is not in the index, so
        // its `-` lines cannot leak a secret into the bundle (#447).
        let index = repo.index()?;
        let mut cached: Option<(Vec<u8>, bool)> = None;
        let mut patch = String::new();
        let mut truncated = false;
        diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            if truncated {
                return true;
            }
            let allowed = match delta.new_file().path() {
                Some(path) if delta.status() != git2::Delta::Deleted => {
                    let key = path.as_os_str().as_encoded_bytes();
                    match &cached {
                        Some((k, ok)) if k == key => *ok,
                        _ => {
                            let ok = Language::from_path(path) != Language::Unknown
                                && index.get_path(path, 0).is_some();
                            cached = Some((key.to_vec(), ok));
                            ok
                        }
                    }
                }
                _ => false,
            };
            if !allowed {
                return true;
            }
            // Invalid UTF-8 bytes become U+FFFD instead of dropping the line.
            let s = String::from_utf8_lossy(line.content());
            let s = s.as_ref();
            // content() omits the +/-/space origin char for body lines; header
            // ('F'/'H'), binary ('B') and no-newline ('=', '>', '<') lines
            // already carry their full text.
            let prefix = match line.origin() {
                o @ ('+' | '-' | ' ') => Some(o),
                _ => None,
            };
            let extra = prefix.map_or(0, char::len_utf8);
            if patch.len() + extra + s.len() > DIFF_MAX_BYTES {
                truncated = true;
            } else {
                patch.extend(prefix);
                patch.push_str(s);
            }
            true
        })?;
        if truncated {
            patch.push_str("\n[diff truncated]\n");
        }
        Ok(patch)
    }

    /// Walk all tracked files in HEAD, returning (relative_path, Language, blob_sha, size).
    ///
    /// The sha and size describe the working-tree content (git blob hash), so
    /// uncommitted edits are noticed and the sha equals HEAD's for an unchanged
    /// file. Unknown-language files keep HEAD's values; a file missing or
    /// unreadable in the working tree gets an `unreadable:`-prefixed sha so it is
    /// never treated as unchanged.
    #[cfg(test)]
    pub(crate) fn scan(&self) -> Result<Vec<ScanEntry>> {
        Ok(self.scan_with_skipped()?.0)
    }

    /// Like [`scan`](Self::scan), but also returns how many tracked files were
    /// skipped because their path is not valid UTF-8 (each one is warned
    /// about individually; the scan continues, #308).
    pub(crate) fn scan_with_skipped(&self) -> Result<(Vec<ScanEntry>, usize)> {
        let acc = self.scan_limited(MAX_SCAN_DEPTH)?;
        Ok((acc.results, acc.skipped))
    }

    /// Walk HEAD descending at most `max_depth` directory levels below the root.
    /// Split from [`scan_with_skipped`](Self::scan_with_skipped) so tests can use a
    /// small limit.
    fn scan_limited(&self, max_depth: usize) -> Result<ScanAcc> {
        let repo = git2::Repository::open(&self.root)
            .with_context(|| format!("Failed to open git repo at {:?}", self.root))?;
        let head = match repo.head() {
            Ok(h) => h.peel_to_tree()?,
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {
                return Err(unborn_head_error(&self.root))
            }
            Err(e) => return Err(e.into()),
        };
        let mut acc = ScanAcc {
            results: Vec::new(),
            skipped: 0,
            too_deep: 0,
            max_depth,
        };
        let index = std::cell::OnceCell::new();
        self.scan_tree(&repo, &head, Vec::new(), &mut acc, &index, 0)?;
        Ok(acc)
    }

    /// Pre-order walk keeping the path as raw bytes so a non-UTF-8 name only
    /// affects its own entries instead of aborting the whole walk. Directories
    /// deeper than `acc.max_depth` are skipped with a warning (#568).
    fn scan_tree(
        &self,
        repo: &git2::Repository,
        tree: &git2::Tree<'_>,
        prefix: Vec<u8>,
        acc: &mut ScanAcc,
        index: &std::cell::OnceCell<Option<git2::Index>>,
        depth: usize,
    ) -> Result<()> {
        for entry in tree.iter() {
            let mut full = prefix.clone();
            full.extend_from_slice(entry.name_bytes());
            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    if depth >= acc.max_depth {
                        eprintln!("{}", too_deep_warning(&full, acc.max_depth));
                        acc.too_deep += 1;
                        continue;
                    }
                    full.push(b'/');
                    let sub = entry.to_object(repo)?.peel_to_tree()?;
                    self.scan_tree(repo, &sub, full, acc, index, depth + 1)?;
                }
                Some(git2::ObjectType::Blob) => {
                    let Ok(name) = String::from_utf8(full) else {
                        eprintln!(
                            "warn: skip non-UTF-8 path in {:?}",
                            String::from_utf8_lossy(&prefix)
                        );
                        acc.skipped += 1;
                        continue;
                    };
                    let rel = PathBuf::from(name);
                    let lang = Language::from_path(&rel);
                    let mut sha = entry.id().to_string();
                    // Header only: avoids loading every blob's content just for its size.
                    let mut size = repo
                        .odb()
                        .and_then(|odb| odb.read_header(entry.id()))
                        .map(|(size, _)| size as i64)
                        .unwrap_or(0);
                    if lang != Language::Unknown
                        && !is_tracked_in_index(
                            &self.canonical_root,
                            &self.canonical_root.join(&rel),
                            index,
                        )
                    {
                        // In HEAD but not in the git index (`git rm --cached`): never hash
                        // or read the working-tree file. The sentinel makes the build step
                        // re-check, be refused, and drop any rows kept from before (#619).
                        sha = format!("unreadable:{sha}");
                    } else if lang != Language::Unknown {
                        match read_bytes_within_root_with(
                            &self.canonical_root,
                            &self.root.join(&rel),
                            index,
                            MAX_FILE_BYTES,
                        ) {
                            Ok(bytes) => {
                                if let Ok(oid) =
                                    git2::Oid::hash_object(git2::ObjectType::Blob, &bytes)
                                {
                                    sha = oid.to_string();
                                    size = bytes.len() as i64;
                                }
                            }
                            Err(e) if is_path_escape(&e) => {
                                eprintln!("warn: skip {:?}: {e}", rel);
                                continue;
                            }
                            // Missing/unreadable in the working tree: HEAD's sha
                            // would look "unchanged" and keep stale rows. A
                            // sentinel forces a re-read that fails and drops them.
                            Err(_) => sha = format!("unreadable:{sha}"),
                        }
                    }
                    acc.results.push((rel, lang, sha, size));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// (relative_path, language, blob_sha, size) as returned by [`GitRepo::scan`].
pub(crate) type ScanEntry = (PathBuf, Language, String, i64);

/// Maximum directory depth `scan_tree` recurses into. Real repos nest a few dozen
/// levels; a hostile tree nested tens of thousands deep overflows the stack (#568).
/// 256 keeps the recursion to ~256 frames (well under 1 MiB even in debug builds,
/// far below the ~15000 levels that still worked), so it fits any thread stack.
const MAX_SCAN_DEPTH: usize = 256;

/// Mutable state threaded through `scan_tree`.
struct ScanAcc {
    results: Vec<ScanEntry>,
    /// Entries skipped for a non-UTF-8 path.
    skipped: usize,
    /// Subtrees skipped for exceeding `max_depth` (each is also warned about; the
    /// count is only read by tests).
    #[cfg_attr(not(test), allow(dead_code))]
    too_deep: usize,
    max_depth: usize,
}

/// The "no commits yet" error. The root is a directory name, so control
/// characters are escaped before it reaches a terminal (#566).
fn unborn_head_error(root: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "'{}' has no commits yet (HEAD is unborn); make a first commit before indexing",
        path_kind::escape_control(&root.to_string_lossy())
    )
}

/// `path` is a raw directory path from the scanned repo (maybe not UTF-8), so
/// control characters are escaped before it reaches a terminal (#599).
fn too_deep_warning(path: &[u8], max_depth: usize) -> String {
    format!(
        "warn: {}: directory nesting deeper than {max_depth}, subtree skipped",
        path_kind::escape_control(&String::from_utf8_lossy(path))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::commit_files;
    use tempfile::TempDir;

    /// #567: a file larger than the cap is refused before it is read.
    #[test]
    fn read_bytes_within_root_refuses_file_over_max_bytes() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("ok.py"), b"12345678").unwrap();
        std::fs::write(root.join("big.py"), b"123456789").unwrap();
        let read = |name: &str| {
            read_bytes_within_root_with(&root, &root.join(name), &std::cell::OnceCell::new(), 8)
        };
        assert_eq!(read("ok.py").unwrap(), b"12345678");
        let err = read("big.py").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::FileTooLarge);
        assert!(!is_path_escape(&err));
    }

    /// #494: a FIFO must be refused without blocking on open/read.
    #[cfg(unix)]
    #[test]
    fn read_bytes_within_root_skips_fifo_without_blocking() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fifo = root.join("pipe.py");
        match std::process::Command::new("mkfifo").arg(&fifo).status() {
            Ok(st) if st.success() => {}
            _ => return, // mkfifo unavailable: skip
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let r = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_bytes_within_root(&r, &r.join("pipe.py")));
        });
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("read blocked on FIFO");
        assert!(res.is_err());
    }

    /// #617: a regular file swapped for a FIFO between the lstat and the open
    /// must be refused without blocking (a plain `open(2)` of a FIFO with no
    /// writer waits forever).
    #[cfg(unix)]
    #[test]
    fn open_verified_refuses_file_swapped_for_fifo_without_blocking() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("a.py");
        std::fs::write(&target, "x = 1\n").unwrap();
        // The lstat the scan took while `a.py` was still a regular file.
        let meta = std::fs::symlink_metadata(&target).unwrap();
        std::fs::remove_file(&target).unwrap();
        match std::process::Command::new("mkfifo").arg(&target).status() {
            Ok(st) if st.success() => {}
            _ => return, // mkfifo unavailable: skip
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let t = target.clone();
        std::thread::spawn(move || {
            let _ = tx.send(open_verified(&t, &meta).map(|_| ()));
        });
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("open_verified blocked on a FIFO swapped in after the check");
        let err = res.unwrap_err();
        assert!(is_path_escape(&err), "expected PathEscape, got {err}");
    }

    /// #543: the file checked by path must be the file actually opened. If the
    /// checked path is swapped for a symlink (to outside the repo) after the
    /// metadata was taken, the open is refused instead of following the link.
    #[cfg(unix)]
    #[test]
    fn open_verified_refuses_file_swapped_for_symlink() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "SECRET").unwrap();
        let target = root.join("a.py");
        std::fs::write(&target, "x = 1\n").unwrap();

        // Unswapped: opens and reads the checked file.
        let meta = std::fs::symlink_metadata(&target).unwrap();
        let mut f = open_verified(&target, &meta).unwrap();
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut f, &mut buf).unwrap();
        assert_eq!(buf, "x = 1\n");

        // Swapped between the check and the open.
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&secret, &target).unwrap();
        let err = open_verified(&target, &meta).unwrap_err();
        assert!(is_path_escape(&err), "got {err:?}");
    }

    /// #543: a regular file replaced by a different regular file (new inode)
    /// after the check is refused too.
    #[cfg(unix)]
    #[test]
    fn open_verified_refuses_replaced_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("a.py");
        std::fs::write(&target, "x = 1\n").unwrap();
        let meta = std::fs::symlink_metadata(&target).unwrap();
        std::fs::write(root.join("b.py"), "y = 2\n").unwrap();
        std::fs::rename(root.join("b.py"), &target).unwrap();
        let err = open_verified(&target, &meta).unwrap_err();
        assert!(is_path_escape(&err), "got {err:?}");
    }

    // ---- GitRepo::new ----

    #[test]
    fn gitrepo_new_nonexistent_path_errors() {
        let tmp = TempDir::new().unwrap();
        let err = GitRepo::new(tmp.path().join("missing")).err().unwrap();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn gitrepo_new_non_git_dir_errors() {
        let tmp = TempDir::new().unwrap();
        let err = GitRepo::new(tmp.path()).err().unwrap();
        assert!(err.to_string().contains("not a git repository"), "{err}");
    }

    #[test]
    fn gitrepo_new_bare_repo_errors() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init_bare(tmp.path()).unwrap();
        let err = GitRepo::new(tmp.path()).err().unwrap();
        assert!(err.to_string().contains("bare repository"), "{err}");
    }

    #[test]
    fn gitrepo_new_valid_repo_exposes_root() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(
            repo.root().canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    // ---- GitRepo::scan ----

    #[test]
    fn scan_returns_nested_files_with_lang_and_size() {
        let tmp = TempDir::new().unwrap();
        commit_files(
            tmp.path(),
            &[("src/main.rs", "fn main() {}\n"), ("notes.txt", "hello")],
            &[],
        );
        let repo = GitRepo::new(tmp.path()).unwrap();
        let mut files = repo.scan().unwrap();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, PathBuf::from("notes.txt"));
        assert_eq!(files[0].1, Language::Unknown);
        assert_eq!(files[0].3, 5);
        assert_eq!(files[1].0, PathBuf::from("src/main.rs"));
        assert_eq!(files[1].1, Language::Rust);
        assert_eq!(files[1].3, 13);
        assert_eq!(files[1].2.len(), 40, "blob sha should be 40 hex chars");
    }

    #[test]
    fn scan_size_matches_blob_size_for_text_and_binary() {
        let tmp = TempDir::new().unwrap();
        // Unknown-language entries keep HEAD's size, so this exercises the
        // header-only path directly.
        commit_files(
            tmp.path(),
            &[("data.bin", "\0\u{1}\0binary\0"), ("empty.dat", "")],
            &[],
        );
        let repo = GitRepo::new(tmp.path()).unwrap();
        let mut files = repo.scan().unwrap();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(files[0].0, PathBuf::from("data.bin"));
        assert_eq!(files[0].3, 10);
        assert_eq!(files[1].0, PathBuf::from("empty.dat"));
        assert_eq!(files[1].3, 0);
    }

    #[test]
    fn scan_without_commits_errors() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.scan().unwrap_err().to_string();
        assert!(err.contains("no commits"), "{err}");
        assert!(!err.contains("UnbornBranch"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn scan_without_commits_escapes_control_chars_in_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("a\u{1b}[2Kb\u{7}");
        std::fs::create_dir(&dir).unwrap();
        git2::Repository::init(&dir).unwrap();
        let repo = GitRepo::new(&dir).unwrap();
        let err = repo.scan().unwrap_err().to_string();
        assert!(err.contains("no commits"), "{err:?}");
        assert!(!err.chars().any(char::is_control), "{err:?}");
        assert!(err.contains("a\\x1b[2Kb\\x07"), "{err:?}");
    }

    #[test]
    fn recent_diff_without_commits_is_empty() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(repo.recent_diff(3).unwrap(), "");
    }

    /// Commit a git tree nested `levels` directories deep (`d/d/.../d`), with a
    /// `f.txt` blob at every level, built bottom-up through the object database so
    /// no working-tree path (and no PATH_MAX limit) is involved.
    fn commit_deep_tree(dir: &Path, levels: usize) {
        let repo = git2::Repository::init(dir).unwrap();
        let blob = repo.blob(b"x").unwrap();
        let mut child: Option<git2::Oid> = None;
        for _ in 0..levels {
            let mut tb = repo.treebuilder(None).unwrap();
            tb.insert("f.txt", blob, 0o100644).unwrap();
            if let Some(c) = child {
                tb.insert("d", c, 0o040000).unwrap();
            }
            child = Some(tb.write().unwrap());
        }
        let tree = repo.find_tree(child.unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "deep", &tree, &[])
            .unwrap();
    }

    /// #568: a tracked tree nested far deeper than any real repo must not
    /// overflow the stack; the default limit applies.
    #[test]
    fn scan_survives_tree_nested_beyond_stack_limit() {
        let tmp = TempDir::new().unwrap();
        commit_deep_tree(tmp.path(), 20_000);
        let repo = GitRepo::new(tmp.path()).unwrap();
        // 1 MiB (half the default test-thread stack) shows the limit, not the
        // stack size, is what keeps the recursion bounded.
        let files = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || repo.scan_with_skipped().unwrap().0)
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(files.len(), MAX_SCAN_DEPTH + 1);
    }

    /// #568: with a small limit, files at or above it are kept, the subtree below
    /// it is skipped exactly once, and the scan still succeeds.
    #[test]
    fn scan_limited_keeps_files_within_limit_and_counts_skipped_subtree() {
        let tmp = TempDir::new().unwrap();
        commit_deep_tree(tmp.path(), 10);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let acc = repo.scan_limited(3).unwrap();
        let mut paths: Vec<_> = acc.results.iter().map(|e| e.0.clone()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("d/d/d/f.txt"),
                PathBuf::from("d/d/f.txt"),
                PathBuf::from("d/f.txt"),
                PathBuf::from("f.txt"),
            ]
        );
        assert_eq!(acc.too_deep, 1);

        // A limit at least as deep as the tree skips nothing.
        let acc = repo.scan_limited(10).unwrap();
        assert_eq!(acc.results.len(), 10);
        assert_eq!(acc.too_deep, 0);
    }

    /// #599: the scan's nesting-depth warning names a repo directory, so its
    /// control characters must be escaped too.
    #[test]
    fn too_deep_warning_escapes_control_characters_in_the_path() {
        let msg = too_deep_warning(b"d\x1b[2J/a\x07b", 256);
        assert!(!msg.chars().any(path_kind::is_ctrl), "{msg:?}");
        assert!(msg.contains("d\\x1b[2J/a\\x07b"), "{msg:?}");
        assert!(msg.contains("directory nesting deeper than 256"), "{msg:?}");
    }

    #[test]
    fn unborn_head_error_escapes_control_characters_in_the_root() {
        let msg = unborn_head_error(Path::new("/r\u{1b}]0;x\u{7}")).to_string();
        assert!(!msg.chars().any(path_kind::is_ctrl), "{msg:?}");
        assert!(msg.contains("/r\\x1b]0;x\\x07"), "{msg:?}");
        assert!(msg.contains("has no commits yet"), "{msg:?}");
    }

    #[test]
    fn scan_ignores_gitlink_entries() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "fn a() {}\n")], &[]);
        // Add a gitlink (submodule, mode 160000) pointing at an arbitrary oid.
        let git = git2::Repository::open(tmp.path()).unwrap();
        let mut index = git.index().unwrap();
        let entry = git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: git2::Oid::from_str("1234567890123456789012345678901234567890").unwrap(),
            flags: 0,
            flags_extended: 0,
            path: b"sub".to_vec(),
        };
        index.add(&entry).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        assert!(
            tree.iter().any(|e| e.name_bytes() == b"sub"),
            "gitlink must be in HEAD tree"
        );
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "add gitlink", &tree, &[&parent])
            .unwrap();

        let repo = GitRepo::new(tmp.path()).unwrap();
        let names: Vec<String> = repo
            .scan()
            .unwrap()
            .into_iter()
            .map(|(p, ..)| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.rs".to_string()]);

        let db = crate::index_builder::IndexDb::open_in_memory().unwrap();
        let stats = crate::index_builder::build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_total, 1);
    }

    // ---- GitRepo::recent_diff ----

    #[test]
    // DiffFormat::Patch callback content() excludes the +/-/space origin char;
    // recent_diff re-adds it from line.origin().
    fn recent_diff_root_commit_shows_initial_files() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "first\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("\n+first\n"), "{diff}");
    }

    /// #447: a commit that deletes a secret must not leak its `-` lines, and
    /// files of unknown language are left out of the diff.
    #[test]
    fn recent_diff_omits_deleted_files_and_unknown_language() {
        let tmp = TempDir::new().unwrap();
        commit_files(
            tmp.path(),
            &[
                ("keep.rs", "fn a() {}\n"),
                ("secret.rs", "const K: &str = \"fake-removed-secret\";\n"),
            ],
            &[],
        );
        commit_files(
            tmp.path(),
            &[
                ("notes.txt", "fake-unknown-lang\n"),
                ("keep.rs", "fn b() {}\n"),
            ],
            &["secret.rs"],
        );
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("+fn b() {}"), "{diff}");
        assert!(!diff.contains("fake-removed-secret"), "{diff}");
        assert!(!diff.contains("secret.rs"), "{diff}");
        assert!(!diff.contains("fake-unknown-lang"), "{diff}");
    }

    #[test]
    fn recent_diff_prefixes_added_removed_and_context_lines() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "keep\nold\ntail\n")], &[]);
        commit_files(tmp.path(), &[("a.rs", "keep\nnew\ntail\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("\n-old\n"), "{diff}");
        assert!(diff.contains("\n+new\n"), "{diff}");
        assert!(diff.contains("\n keep\n"), "{diff}");
        assert!(diff.contains("\n tail\n"), "{diff}");
        // File/hunk headers are not double-prefixed.
        assert!(diff.starts_with("diff --git"), "{diff}");
        assert!(diff.contains("\n@@ "), "{diff}");
    }

    #[test]
    fn recent_diff_keeps_no_newline_marker_unprefixed() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "old")], &[]);
        commit_files(tmp.path(), &[("a.rs", "new")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(
            diff.contains("-old\n\\ No newline at end of file\n"),
            "{diff}"
        );
        assert!(
            diff.contains("+new\n\\ No newline at end of file\n"),
            "{diff}"
        );
    }

    #[test]
    fn recent_diff_cap_counts_origin_byte() {
        // Body lines are 3 bytes ("+x\n"). A first line of 0..=2 extra bytes
        // shifts where the cap lands relative to the line stride; for one of
        // the three shifts the patch stops exactly 2 bytes short of the cap,
        // where a check that ignored the origin byte would accept one more
        // line and overshoot the cap by 1.
        for pad in ["", "y", "yy"] {
            let tmp = TempDir::new().unwrap();
            let big = format!("{pad}\n{}", "x\n".repeat(300_000));
            commit_files(tmp.path(), &[("n.rs", big.as_str())], &[]);
            let repo = GitRepo::new(tmp.path()).unwrap();
            let diff = repo.recent_diff(1).unwrap();
            let body = diff.strip_suffix("\n[diff truncated]\n").expect("marker");
            assert!(body.len() <= 512 * 1024, "pad {pad:?}: {}", body.len());
        }
    }

    #[test]
    fn recent_diff_covers_last_n_commits_only() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.rs", "two\n")], &[]);
        commit_files(tmp.path(), &[("c.rs", "three\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("three"));
        assert!(!diff.contains("two"));
        let diff2 = repo.recent_diff(2).unwrap();
        assert!(diff2.contains("three") && diff2.contains("two"));
        assert!(!diff2.contains("one"));
    }

    /// #465: in a shallow clone the boundary commit is not the true root, so
    /// the diff must not fall back to the empty tree (whole repo).
    #[test]
    fn recent_diff_shallow_clone_does_not_include_whole_repo() {
        let src = TempDir::new().unwrap();
        for (f, c) in [
            ("a.rs", "one"),
            ("b.rs", "two"),
            ("c.rs", "three"),
            ("d.rs", "four"),
        ] {
            commit_files(src.path(), &[(f, &format!("{c}\n"))], &[]);
        }
        let dst = TempDir::new().unwrap();
        let clone = dst.path().join("clone");
        let url = format!("file://{}", src.path().display());
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth", "2", &url])
            .arg(&clone)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let repo = GitRepo::new(&clone).unwrap();
        let diff = repo.recent_diff(3).unwrap();
        assert!(diff.contains("+four"), "{diff}");
        assert!(!diff.contains("+one") && !diff.contains("+two"), "{diff}");
        assert!(!diff.contains("+three"), "{diff}");
    }

    /// Runs `git` in `dir` (test helper for the shallow-merge fixtures).
    fn git_cli(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// R -> M1 -> Merge(second parent: S1..S5 forked from R), cloned with depth 3.
    /// First-parent reaches the true root R while the second-parent side ends at a
    /// shallow boundary, so the repo is shallow but R is a real root.
    fn shallow_merge_clone(dst: &Path) -> std::path::PathBuf {
        let src = dst.join("src");
        std::fs::create_dir_all(&src).unwrap();
        git_cli(&src, &["init", "-q", "-b", "main"]);
        commit_files(&src, &[("root.rs", "root_line\n")], &[]);
        git_cli(&src, &["checkout", "-q", "-b", "side"]);
        for i in 1..=5 {
            commit_files(&src, &[(&format!("s{i}.rs"), "side\n")], &[]);
        }
        git_cli(&src, &["checkout", "-q", "main"]);
        commit_files(&src, &[("m1.rs", "m1_line\n")], &[]);
        git_cli(&src, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
        let clone = dst.join("clone");
        let url = format!("file://{}", src.display());
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth", "3", &url])
            .arg(&clone)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        clone
    }

    /// #494: a shallow second-parent side must not make a true first-parent root
    /// look like a shallow boundary (its files would drop out of the diff).
    #[test]
    fn recent_diff_true_root_in_shallow_merge_clone_includes_root_files() {
        let dst = TempDir::new().unwrap();
        let clone = shallow_merge_clone(dst.path());
        assert!(git2::Repository::open(&clone).unwrap().is_shallow());
        let diff = GitRepo::new(&clone).unwrap().recent_diff(3).unwrap();
        assert!(diff.contains("+root_line"), "{diff}");
    }

    /// #494: depth 1 makes HEAD itself the boundary; the empty diff is explained on stderr
    /// by the code path, and must still be empty rather than the whole repo.
    #[test]
    fn recent_diff_depth_one_clone_is_empty() {
        let src = TempDir::new().unwrap();
        commit_files(src.path(), &[("a.rs", "one\n")], &[]);
        commit_files(src.path(), &[("b.rs", "two\n")], &[]);
        let dst = TempDir::new().unwrap();
        let clone = dst.path().join("clone");
        let url = format!("file://{}", src.path().display());
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth", "1", &url])
            .arg(&clone)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let diff = GitRepo::new(&clone).unwrap().recent_diff(3).unwrap();
        assert!(!diff.contains("+one"), "{diff}");
    }

    #[test]
    fn recent_diff_n_larger_than_history_clamps_to_root() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.rs", "two\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(100).unwrap();
        assert!(diff.contains("two"), "{diff}");
        assert!(diff.contains("+one"), "{diff}");
    }

    #[test]
    fn recent_diff_n_larger_than_three_commit_history_includes_root_files() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.rs", "two\n")], &[]);
        commit_files(tmp.path(), &[("c.rs", "three\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(5).unwrap();
        assert!(
            diff.contains("+one") && diff.contains("+two") && diff.contains("+three"),
            "{diff}"
        );
        // n = history length: one step past the root, so root files are included.
        let diff3 = repo.recent_diff(3).unwrap();
        assert!(diff3.contains("+one"), "{diff3}");
        // Exactly reaching the root (n = commits - 1) still excludes root files.
        let diff2 = repo.recent_diff(2).unwrap();
        assert!(!diff2.contains("+one"), "{diff2}");
    }

    #[test]
    fn recent_diff_single_commit_with_large_n_shows_files() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "first\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert!(repo.recent_diff(5).unwrap().contains("+first"));
    }

    #[test]
    fn recent_diff_zero_commits_is_empty() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.rs", "two\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(repo.recent_diff(0).unwrap(), "");
    }

    #[test]
    fn recent_diff_zero_commits_is_empty_for_root_only_history() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "first\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(repo.recent_diff(0).unwrap(), "");
    }

    #[test]
    fn recent_diff_truncates_large_patch_with_marker() {
        let tmp = TempDir::new().unwrap();
        let big = "0123456789abcdef\n".repeat(40_000); // ~680 KB > 512 KiB cap
        commit_files(tmp.path(), &[("big.rs", big.as_str())], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.ends_with("[diff truncated]\n"));
        assert!(diff.len() <= 512 * 1024 + "\n[diff truncated]\n".len());
    }

    #[test]
    fn recent_diff_converts_non_utf8_lines_lossily() {
        let tmp = TempDir::new().unwrap();
        let repo_git = git2::Repository::init(tmp.path()).unwrap();
        // No NUL byte, so git treats the file as text; line 2 is invalid UTF-8.
        std::fs::write(tmp.path().join("m.rs"), b"good1\nbad\xff\xfe\ngood2\n").unwrap();
        let mut index = repo_git.index().unwrap();
        index.add_path(Path::new("m.rs")).unwrap();
        index.write().unwrap();
        let tree = repo_git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        repo_git
            .commit(Some("HEAD"), &sig, &sig, "bin", &tree, &[])
            .unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        // The invalid line is kept with U+FFFD replacements; no marker.
        assert!(diff.contains("\n+good1\n"), "{diff}");
        assert!(diff.contains("\n+good2\n"), "{diff}");
        assert!(diff.contains("\n+bad\u{FFFD}\u{FFFD}\n"), "{diff}");
        assert!(!diff.contains("[diff truncated]"), "{diff}");
    }

    #[test]
    fn recent_diff_under_cap_has_no_truncation_marker() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "small\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert!(!repo.recent_diff(1).unwrap().contains("[diff truncated]"));
    }

    #[cfg(unix)]
    fn symlink_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("real.py"), "def real(): pass\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap(); // never added
        std::os::unix::fs::symlink(".env", tmp.path().join("a.py")).unwrap();
        std::os::unix::fs::symlink("real.py", tmp.path().join("b.py")).unwrap();
        let mut index = git.index().unwrap();
        for f in ["real.py", "a.py", "b.py"] {
            index.add_path(Path::new(f)).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        tmp
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_symlink_to_untracked_file() {
        let tmp = symlink_repo();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.read_tracked(Path::new("a.py")).unwrap_err();
        assert!(is_path_escape(&err), "{err}");
        assert!(err.to_string().contains("untracked"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_allows_symlink_to_tracked_file() {
        let tmp = symlink_repo();
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(
            repo.read_tracked(Path::new("b.py")).unwrap(),
            "def real(): pass\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_resolves_many_tracked_symlinks() {
        let tmp = symlink_repo();
        let git = git2::Repository::open(tmp.path()).unwrap();
        let mut index = git.index().unwrap();
        for i in 0..50 {
            let name = format!("link{i}.py");
            std::os::unix::fs::symlink("real.py", tmp.path().join(&name)).unwrap();
            index.add_path(Path::new(&name)).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "links", &tree, &[&parent])
            .unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let names: Vec<String> = repo
            .scan()
            .unwrap()
            .into_iter()
            .map(|(p, ..)| p.to_string_lossy().into_owned())
            .collect();
        for i in 0..50 {
            assert!(names.contains(&format!("link{i}.py")), "{names:?}");
        }
        assert!(!names.contains(&"a.py".to_string()), "{names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlink_to_untracked_file() {
        let tmp = symlink_repo();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let names: Vec<String> = repo
            .scan()
            .unwrap()
            .into_iter()
            .map(|(p, ..)| p.to_string_lossy().into_owned())
            .collect();
        assert!(!names.contains(&"a.py".to_string()), "{names:?}");
        assert!(names.contains(&"b.py".to_string()), "{names:?}");
        assert!(names.contains(&"real.py".to_string()), "{names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_file_via_symlinked_directory_to_untracked() {
        let tmp = TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        std::fs::create_dir(tmp.path().join("ignored_dir")).unwrap();
        std::fs::write(tmp.path().join("ignored_dir/secret.py"), "S=1\n").unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored_dir/\n").unwrap();
        std::os::unix::fs::symlink("ignored_dir", tmp.path().join("docs")).unwrap();
        let mut index = git.index().unwrap();
        for f in [".gitignore", "docs"] {
            index.add_path(Path::new(f)).unwrap();
        }
        index.write().unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.read_tracked(Path::new("docs/secret.py")).unwrap_err();
        assert!(is_path_escape(&err), "{err}");
    }

    #[cfg(unix)]
    fn commit_dir_link(link: &str, target: &str, extra_dir: Option<&str>) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(tmp.path().join(".gitignore"), ".env\n").unwrap();
        if let Some(d) = extra_dir {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        std::os::unix::fs::symlink(target, tmp.path().join(link)).unwrap();
        let mut index = git.index().unwrap();
        for f in [".gitignore", link] {
            index.add_path(Path::new(f)).unwrap();
        }
        index.write().unwrap();
        tmp
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_via_dir_symlink_to_root() {
        let tmp = commit_dir_link("docs", ".", None);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.read_tracked(Path::new("docs/.env")).unwrap_err();
        assert!(is_path_escape(&err), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_via_nested_dir_symlink_to_parent() {
        let tmp = commit_dir_link("a/b", "..", Some("a"));
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.read_tracked(Path::new("a/b/.env")).unwrap_err();
        assert!(is_path_escape(&err), "{err}");
    }
}
