//! Walks the project and figures out what every tracked file contains right now.

use crate::store::{Index, IndexEntry, Repo};
use anyhow::{Result, bail};
use ignore::WalkBuilder;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// Folders that are almost always generated or vendored. Skipped even without a .gitignore.
const SKIP_DIRS: &[&str] = &[
    ".git", ".rewind", ".hg", ".svn", ".jj", "node_modules", ".venv", "venv", "__pycache__",
    ".next", ".nuxt", ".svelte-kit", ".turbo", ".cache", ".parcel-cache", ".gradle",
    ".mypy_cache", ".pytest_cache", ".ruff_cache", ".tox", "target", ".direnv", ".terraform",
];

pub const MAX_FILE: u64 = 20 * 1024 * 1024;

pub struct Scan {
    pub index: Index,
    /// Files left out because they are larger than MAX_FILE.
    pub skipped: Vec<String>,
}

/// Scan the working tree. Files whose size and mtime match `prev` reuse its hash.
/// With `store`, new contents are written to the object store.
pub fn scan(repo: &Repo, prev: &Index, store: bool, limit: Option<usize>) -> Result<Scan> {
    let mut wb = WalkBuilder::new(&repo.root);
    wb.hidden(false)
        .require_git(false)
        .follow_links(false)
        .add_custom_ignore_filename(".rewindignore")
        .filter_entry(|e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            !(is_dir && e.depth() > 0 && SKIP_DIRS.contains(&e.file_name().to_str().unwrap_or("")))
        });

    let mut index = Index::new();
    let mut skipped = Vec::new();
    for ent in wb.build() {
        let Ok(ent) = ent else { continue };
        if !ent.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = ent.path().strip_prefix(&repo.root) else { continue };
        let Some(rel) = rel.to_str() else {
            skipped.push(rel.to_string_lossy().into_owned());
            continue;
        };
        let Ok(md) = ent.metadata() else { continue };
        let size = md.len();
        if size > MAX_FILE {
            skipped.push(rel.to_string());
            continue;
        }
        let mtime = md.mtime() * 1_000_000_000 + md.mtime_nsec();
        let mode = md.permissions().mode() & 0o777;
        if let Some(p) = prev.get(rel)
            && p.size == size
            && p.mtime == mtime
            && p.mode == mode
        {
            index.insert(rel.to_string(), p.clone());
            continue;
        }
        // Vanished or unreadable since the walk saw it: treat as not there.
        let Ok(data) = fs::read(ent.path()) else { continue };
        let hash = if store { repo.put(&data)? } else { blake3::hash(&data).to_hex().to_string() };
        index.insert(rel.to_string(), IndexEntry { hash, size: data.len() as u64, mtime, mode });
        if let Some(l) = limit
            && index.len() > l
        {
            bail!(
                "this folder has more than {l} files; that looks like more than one project.\n  \
                 cd into the project, add a .rewindignore, or pass --force"
            );
        }
    }
    Ok(Scan { index, skipped })
}
