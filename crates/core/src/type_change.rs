//! What a managed target IS (nothing, a file, a folder, a link) and how a
//! folder in the source compares with the folder now on disk. Pure model:
//! the scanner decides what to do with it.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use crate::drift::ContentHash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Absent,
    File,
    Folder,
    /// The link text as written, not resolved.
    Link(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeChange {
    pub source: EntryKind,
    pub disk: EntryKind,
    /// False means the source changed and the disk is what chezmoi last wrote.
    pub disk_moved: bool,
}

/// What is at `path` right now. Never follows a link: a link to a folder is a
/// `Link`, a dangling link is a `Link`.
pub fn disk_kind(path: &Path) -> io::Result<EntryKind> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(EntryKind::Absent),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        let text = std::fs::read_link(path)?;
        Ok(EntryKind::Link(text.to_string_lossy().into_owned()))
    } else if meta.is_dir() {
        Ok(EntryKind::Folder)
    } else {
        Ok(EntryKind::File)
    }
}

fn kind_name(kind: &EntryKind) -> Option<&'static str> {
    match kind {
        EntryKind::Absent => None,
        EntryKind::File => Some("file"),
        EntryKind::Folder => Some("dir"),
        EntryKind::Link(_) => Some("symlink"),
    }
}

/// The type change between what the source says and what the disk holds, or
/// `None` when they agree. A deleted file or folder (`Absent` disk under a
/// `File` or `Folder` source) is not a type change: the per-file deleted rows
/// own it. `state_kind` is the kind chezmoi last wrote; the disk moved when it
/// no longer matches. The state holds no link text, so two different links
/// always read as the disk moving.
pub fn type_change(
    source: &EntryKind,
    disk: &EntryKind,
    state_kind: Option<&str>,
) -> Option<TypeChange> {
    if source == disk {
        return None;
    }
    if matches!(disk, EntryKind::Absent) && matches!(source, EntryKind::File | EntryKind::Folder) {
        return None;
    }
    let both_links = matches!((source, disk), (EntryKind::Link(_), EntryKind::Link(_)));
    Some(TypeChange {
        source: source.clone(),
        disk: disk.clone(),
        disk_moved: both_links || state_kind != kind_name(disk),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompareGroup {
    OnlyInSource,
    Differs,
    OnlyOnDisk,
    Same,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderComparison {
    /// Relative paths, sorted by group (source-only, differs, disk-only,
    /// same), then by path. Disk-only entries are top-level names; a folder
    /// has a trailing `/`.
    pub entries: Vec<(PathBuf, CompareGroup)>,
}

/// Compare the source version of a folder (`source`: relative path and
/// content hash of each file) with the folder at `disk_root`. A link on the
/// disk side is followed on purpose: that is the content the user now has.
pub fn compare_folder(
    source: &[(PathBuf, ContentHash)],
    disk_root: &Path,
) -> io::Result<FolderComparison> {
    let mut entries = Vec::new();
    let mut source_tops = BTreeSet::new();
    for (rel, hash) in source {
        if let Some(first) = rel.components().next() {
            source_tops.insert(PathBuf::from(first.as_os_str()));
        }
        match std::fs::read(disk_root.join(rel)) {
            Ok(bytes) if ContentHash::of(&bytes) == *hash => {
                entries.push((rel.clone(), CompareGroup::Same));
            }
            Ok(_) => entries.push((rel.clone(), CompareGroup::Differs)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                entries.push((rel.clone(), CompareGroup::OnlyInSource));
            }
            Err(e) => return Err(e),
        }
    }
    let read = match std::fs::read_dir(disk_root) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            entries.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
            return Ok(FolderComparison { entries });
        }
        Err(e) => return Err(e),
    };
    for item in read {
        let item = item?;
        let name = PathBuf::from(item.file_name());
        if source_tops.contains(&name) {
            continue;
        }
        let is_dir = std::fs::metadata(item.path()).is_ok_and(|m| m.is_dir());
        let shown = if is_dir {
            PathBuf::from(format!("{}/", name.to_string_lossy()))
        } else {
            name
        };
        entries.push((shown, CompareGroup::OnlyOnDisk));
    }
    entries.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    Ok(FolderComparison { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// MUTATION: use `std::fs::metadata` instead of `symlink_metadata` → the
    /// link to a directory reads as `Folder` and the dangling link errors.
    #[test]
    fn disk_kind_reports_a_link_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink("real", root.join("l")).unwrap();
        std::os::unix::fs::symlink("gone", root.join("dangling")).unwrap();
        std::fs::write(root.join("f"), "x").unwrap();
        assert_eq!(
            disk_kind(&root.join("l")).unwrap(),
            EntryKind::Link("real".into())
        );
        assert_eq!(
            disk_kind(&root.join("dangling")).unwrap(),
            EntryKind::Link("gone".into())
        );
        assert_eq!(disk_kind(&root.join("real")).unwrap(), EntryKind::Folder);
        assert_eq!(disk_kind(&root.join("f")).unwrap(), EntryKind::File);
        assert_eq!(disk_kind(&root.join("missing")).unwrap(), EntryKind::Absent);
    }

    /// MUTATION: drop the `Absent` exclusion from `type_change` → the
    /// `(File, Absent)` and `(Folder, Absent)` cases return `Some`.
    #[test]
    fn type_change_table() {
        let link = |s: &str| EntryKind::Link(s.into());
        assert_eq!(
            type_change(&EntryKind::Folder, &link(".real"), Some("dir")),
            Some(TypeChange {
                source: EntryKind::Folder,
                disk: link(".real"),
                disk_moved: true
            })
        );
        assert_eq!(
            type_change(&link(".real"), &link(".other"), Some("symlink")).map(|t| t.disk_moved),
            Some(true)
        );
        assert_eq!(
            type_change(&link(".real"), &EntryKind::Folder, Some("dir")).map(|t| t.disk_moved),
            Some(false)
        );
        assert_eq!(
            type_change(&EntryKind::File, &EntryKind::Absent, None),
            None
        );
        assert_eq!(
            type_change(&EntryKind::Folder, &EntryKind::Absent, None),
            None
        );
        assert_eq!(
            type_change(&EntryKind::Folder, &EntryKind::Folder, Some("dir")),
            None
        );
    }

    /// MUTATION: drop the trailing `/` on folder names (`is_dir = false`) →
    /// `sub/` reads `sub`; list nested files instead of top-level names →
    /// `sub/x` replaces `sub/`.
    #[test]
    fn compare_folder_groups_and_orders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("one"), "a").unwrap();
        std::fs::write(root.join("two"), "b2").unwrap();
        std::fs::write(root.join("three"), "t").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/x"), "x").unwrap();
        let source = vec![
            (p("one"), ContentHash::of(b"a")),
            (p("two"), ContentHash::of(b"b")),
            (p("gone"), ContentHash::of(b"c")),
        ];
        let got = compare_folder(&source, root).unwrap();
        // Compared as strings: `PathBuf` equality ignores a trailing `/`.
        let shown: Vec<(String, CompareGroup)> = got
            .entries
            .iter()
            .map(|(path, group)| (path.to_string_lossy().into_owned(), *group))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("gone".to_string(), CompareGroup::OnlyInSource),
                ("two".to_string(), CompareGroup::Differs),
                ("sub/".to_string(), CompareGroup::OnlyOnDisk),
                ("three".to_string(), CompareGroup::OnlyOnDisk),
                ("one".to_string(), CompareGroup::Same),
            ]
        );
    }
}
