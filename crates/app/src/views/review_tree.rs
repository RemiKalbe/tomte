//! The SAFE TO RESOLVE group as a folder tree (spec §2.5): a pure projection
//! of target paths into folder and file rows, rooted at `$HOME` (which is not
//! shown), single-child directory chains compacted, folders before files,
//! case-insensitive. `excluded` are the targets that sit in the other two
//! groups: they are never rows here, but a folder counts them so its chip
//! can say "2 of 3" — what a tick reaches, of what is there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use gpui::SharedString;

pub(super) struct FolderNode {
    /// The absolute directory this row stands for — the DEEPEST directory of
    /// a compacted chain, which is what `collapsed` is keyed by.
    pub path: PathBuf,
    // `depth`, `label`, `safe`, `total` and `collapsed` are read only by
    // Task 7's tree rendering (the row's indent, name, chip and chevron);
    // `review.rs` carries the node through to a `SidebarRow::Folder` without
    // reading them yet, and `path` above is already read there
    // (`safe_descendants`'s folder argument).
    #[allow(dead_code)]
    pub depth: usize,
    /// `.config/nvim/` — the compacted chain, trailing slash.
    #[allow(dead_code)]
    pub label: SharedString,
    #[allow(dead_code)]
    pub safe: usize,
    #[allow(dead_code)]
    pub total: usize,
    #[allow(dead_code)]
    pub collapsed: bool,
}

pub(super) enum TreeRow {
    Folder(FolderNode),
    File { target: PathBuf, depth: usize },
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    /// Safe files directly here: (sort key, target).
    safe_files: Vec<(String, PathBuf)>,
    /// Files here that are NOT safe (needs-you / processing): counted, never listed.
    excluded_here: usize,
    safe: usize,
    total: usize,
}

impl Node {
    fn insert(&mut self, rel: &[String], target: &Path, safe: bool) {
        self.total += 1;
        if safe {
            self.safe += 1;
        }
        match rel {
            [] => unreachable!("a target has a file name"),
            [name] => {
                if safe {
                    self.safe_files
                        .push((name.to_lowercase(), target.to_path_buf()));
                } else {
                    self.excluded_here += 1;
                }
            }
            [dir, rest @ ..] => self
                .dirs
                .entry(dir.clone())
                .or_default()
                .insert(rest, target, safe),
        }
    }

    fn has_files(&self) -> bool {
        !self.safe_files.is_empty() || self.excluded_here > 0
    }
}

/// Split `target` into the components under its root: home-relative when
/// under `home`, else absolute with a leading "" so the first label reads
/// "/etc/". Returns (root-is-slash, components).
fn split(target: &Path, home: &Path) -> (bool, Vec<String>) {
    if let Ok(rel) = target.strip_prefix(home) {
        let comps = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        return (false, comps);
    }
    let comps = target
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    (true, comps)
}

fn build(safe: &[PathBuf], excluded: &[PathBuf], home: &Path) -> (Node, Node) {
    let mut under_home = Node::default();
    let mut under_root = Node::default();
    for (targets, is_safe) in [(safe, true), (excluded, false)] {
        for t in targets {
            let (slash, comps) = split(t, home);
            if comps.is_empty() {
                continue;
            }
            let node = if slash {
                &mut under_root
            } else {
                &mut under_home
            };
            node.insert(&comps, t, is_safe);
        }
    }
    (under_home, under_root)
}

/// Walk one directory node: emit its (compacted) folder row, then — unless
/// collapsed — its children. `dir_path` is the absolute path of `node`.
fn walk(
    name: &str,
    node: &Node,
    dir_path: PathBuf,
    depth: usize,
    collapsed: &BTreeSet<PathBuf>,
    out: &mut Vec<TreeRow>,
) {
    if node.safe == 0 {
        return;
    }
    // Compact: a directory with no files and exactly one child directory is
    // one row with its child. By structure, never by name.
    let mut label = format!("{name}/");
    let mut node = node;
    let mut path = dir_path;
    while !node.has_files() && node.dirs.len() == 1 {
        let (child_name, child) = node.dirs.iter().next().expect("len checked");
        label.push_str(child_name);
        label.push('/');
        path = path.join(child_name);
        node = child;
    }
    let is_collapsed = collapsed.contains(&path);
    out.push(TreeRow::Folder(FolderNode {
        path: path.clone(),
        depth,
        label: label.into(),
        safe: node.safe,
        total: node.total,
        collapsed: is_collapsed,
    }));
    if is_collapsed {
        return;
    }
    emit_children(node, &path, depth + 1, collapsed, out);
}

fn emit_children(
    node: &Node,
    dir_path: &Path,
    depth: usize,
    collapsed: &BTreeSet<PathBuf>,
    out: &mut Vec<TreeRow>,
) {
    let mut dirs: Vec<(&String, &Node)> = node.dirs.iter().collect();
    dirs.sort_by_key(|(name, _)| name.to_lowercase());
    for (name, child) in dirs {
        walk(name, child, dir_path.join(name), depth, collapsed, out);
    }
    let mut files = node.safe_files.clone();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, target) in files {
        out.push(TreeRow::File { target, depth });
    }
}

pub(super) fn tree_rows(
    safe: &[PathBuf],
    excluded: &[PathBuf],
    collapsed: &BTreeSet<PathBuf>,
    home: &Path,
) -> Vec<TreeRow> {
    let (under_home, under_root) = build(safe, excluded, home);
    let mut out = Vec::new();
    // Under home: the root is not shown; its children start at depth 0.
    emit_children(&under_home, home, 0, collapsed, &mut out);
    // Outside home: each top-level directory reads "/etc/".
    let mut dirs: Vec<(&String, &Node)> = under_root.dirs.iter().collect();
    dirs.sort_by_key(|(name, _)| name.to_lowercase());
    for (name, child) in dirs {
        walk(
            &format!("/{name}"),
            child,
            PathBuf::from("/").join(name),
            0,
            collapsed,
            &mut out,
        );
    }
    out
}

/// Every safe file in tree order, collapse ignored — the order `Selection`
/// keeps members in while the tree is showing.
pub(super) fn tree_order(safe: &[PathBuf], home: &Path) -> Vec<PathBuf> {
    tree_rows(safe, &[], &BTreeSet::new(), home)
        .into_iter()
        .filter_map(|r| match r {
            TreeRow::File { target, .. } => Some(target),
            TreeRow::Folder(_) => None,
        })
        .collect()
}

/// Every safe file under `folder` (an absolute directory), in tree order,
/// collapsed or not — what ticking the folder reaches.
pub(super) fn safe_descendants(safe: &[PathBuf], folder: &Path, home: &Path) -> Vec<PathBuf> {
    tree_order(safe, home)
        .into_iter()
        .filter(|t| t.starts_with(folder))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/Users/x")
    }
    fn h(rel: &str) -> PathBuf {
        home().join(rel)
    }
    fn folders(rows: &[TreeRow]) -> Vec<(String, usize, usize, usize)> {
        rows.iter()
            .filter_map(|r| match r {
                TreeRow::Folder(f) => Some((f.label.to_string(), f.depth, f.safe, f.total)),
                _ => None,
            })
            .collect()
    }
    fn files(rows: &[TreeRow]) -> Vec<(PathBuf, usize)> {
        rows.iter()
            .filter_map(|r| match r {
                TreeRow::File { target, depth } => Some((target.clone(), *depth)),
                _ => None,
            })
            .collect()
    }

    /// MUTATION: compact only by NAME match, or never compact → the first
    /// folder reads ".config/" with "nvim/" under it; compact past a folder
    /// that holds a file → ".config/nvim/lua/" swallows "init.lua"'s parent.
    #[test]
    fn single_child_directory_chains_compact_and_stop_at_a_file() {
        let safe = vec![
            h(".config/nvim/init.lua"),
            h(".config/nvim/lua/plugins.lua"),
        ];
        let rows = tree_rows(&safe, &[], &BTreeSet::new(), &home());
        assert_eq!(
            folders(&rows),
            vec![(".config/nvim/".into(), 0, 2, 2), ("lua/".into(), 1, 1, 1)]
        );
        assert_eq!(
            files(&rows),
            vec![
                (h(".config/nvim/lua/plugins.lua"), 2),
                (h(".config/nvim/init.lua"), 1)
            ],
            "folders first, then files; each at its own depth"
        );
    }

    /// MUTATION: sort case-sensitively → "Zed" sorts before "nvim".
    #[test]
    fn directories_before_files_both_case_insensitive() {
        let safe = vec![h("B.txt"), h("a.txt"), h("Zed/s.json"), h("nvim/i.lua")];
        let rows = tree_rows(&safe, &[], &BTreeSet::new(), &home());
        assert_eq!(
            folders(&rows)
                .iter()
                .map(|f| f.0.clone())
                .collect::<Vec<_>>(),
            vec!["nvim/", "Zed/"]
        );
        let names: Vec<String> = files(&rows)
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // nvim's, then Zed's, then the top-level two — `a.txt` before
        // `B.txt` only under a case-insensitive sort (ASCII puts `B` first).
        assert_eq!(names, vec!["i.lua", "s.json", "a.txt", "B.txt"]);
    }

    /// MUTATION: count `excluded` into `safe` → the folder says 3 of 3.
    #[test]
    fn a_folder_counts_what_a_tick_reaches_of_what_is_there() {
        let safe = vec![h(".config/a"), h(".config/b")];
        let excluded = vec![h(".config/needs-you")];
        let rows = tree_rows(&safe, &excluded, &BTreeSet::new(), &home());
        assert_eq!(folders(&rows), vec![(".config/".into(), 0, 2, 3)]);
        assert_eq!(files(&rows).len(), 2, "the excluded file is not a row here");
    }

    /// MUTATION: emit a folder with no safe descendants → an empty ".other/"
    /// row appears for a folder the user cannot act on.
    #[test]
    fn a_folder_with_nothing_safe_under_it_is_not_shown() {
        let rows = tree_rows(&[h("a")], &[h(".other/x")], &BTreeSet::new(), &home());
        assert!(folders(&rows).is_empty());
    }

    /// MUTATION: `collapsed` hides the folder row itself, or `safe_descendants`
    /// honours `collapsed` → the last assertion fails.
    #[test]
    fn a_collapsed_folder_hides_its_rows_but_a_tick_still_reaches_them() {
        let safe = vec![h(".config/a"), h(".config/b"), h("top")];
        let collapsed = BTreeSet::from([h(".config")]);
        let rows = tree_rows(&safe, &[], &collapsed, &home());
        assert_eq!(folders(&rows), vec![(".config/".into(), 0, 2, 2)]);
        assert!(matches!(&rows[0], TreeRow::Folder(f) if f.collapsed));
        assert_eq!(
            files(&rows),
            vec![(h("top"), 0)],
            "the collapsed folder's files are gone from the plan"
        );
        assert_eq!(
            safe_descendants(&safe, &h(".config"), &home()),
            vec![h(".config/a"), h(".config/b")]
        );
    }

    /// MUTATION: root everything at home → "/etc/hosts" becomes "etc/".
    #[test]
    fn a_target_outside_home_roots_at_slash() {
        let rows = tree_rows(
            &[PathBuf::from("/etc/hosts"), h("a")],
            &[],
            &BTreeSet::new(),
            &home(),
        );
        assert_eq!(folders(&rows), vec![("/etc/".into(), 0, 1, 1)]);
    }

    /// MUTATION: `tree_order` honours a collapse set (it must not take one)
    /// or lists files before folders → the order differs from the rows'.
    #[test]
    fn tree_order_is_the_rows_file_order_with_nothing_collapsed() {
        let safe = vec![h("z"), h(".config/nvim/init.lua"), h(".config/a")];
        let expected: Vec<PathBuf> = files(&tree_rows(&safe, &[], &BTreeSet::new(), &home()))
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(tree_order(&safe, &home()), expected);
        assert_eq!(
            expected,
            vec![h(".config/nvim/init.lua"), h(".config/a"), h("z")]
        );
    }
}
