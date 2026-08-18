//! Virtual tree for repo-relative Git paths. Unlike the Explorer tree, this
//! never reads the filesystem: deleted and historical files must remain rows.

use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Directory {
        path: String,
        name: String,
        depth: usize,
        expanded: bool,
    },
    File {
        index: usize,
        depth: usize,
    },
}

impl Row {
    pub fn depth(&self) -> usize {
        match self {
            Self::Directory { depth, .. } | Self::File { depth, .. } => *depth,
        }
    }

    pub fn expanded(&self) -> Option<bool> {
        match self {
            Self::Directory { expanded, .. } => Some(*expanded),
            Self::File { .. } => None,
        }
    }
}

#[derive(Default)]
struct Node {
    dirs: HashMap<String, Node>,
    files: Vec<(String, usize)>,
}

#[derive(Default)]
pub struct Tree {
    root: Node,
}

impl Tree {
    pub fn new<'a>(paths: impl IntoIterator<Item = (usize, &'a str)>) -> Self {
        let mut tree = Self::default();
        for (index, path) in paths {
            tree.insert(index, path);
        }
        tree
    }

    fn insert(&mut self, index: usize, path: &str) {
        let mut parts = path.split('/').filter(|part| !part.is_empty()).peekable();
        let mut node = &mut self.root;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                node.files.push((part.to_string(), index));
            } else {
                node = node.dirs.entry(part.to_string()).or_default();
            }
        }
    }

    /// Visible rows. Directories are expanded unless their compacted path is
    /// present in `collapsed`, so newly appearing paths are visible by default.
    pub fn rows(&self, collapsed: &BTreeSet<String>) -> Vec<Row> {
        let mut rows = Vec::new();
        walk(&self.root, "", 0, collapsed, &mut rows);
        rows
    }

    pub fn collapse_all(&self, collapsed: &mut BTreeSet<String>) {
        for row in self.rows(&BTreeSet::new()) {
            if let Row::Directory { path, .. } = row {
                collapsed.insert(path);
            }
        }
    }
}

fn walk(
    node: &Node,
    parent: &str,
    depth: usize,
    collapsed: &BTreeSet<String>,
    rows: &mut Vec<Row>,
) {
    let mut dirs: Vec<_> = node.dirs.iter().collect();
    dirs.sort_by(|(a, _), (b, _)| name_cmp(a, b));
    for (first, child) in dirs {
        let mut name = first.clone();
        let mut child = child;
        while child.files.is_empty() && child.dirs.len() == 1 {
            let (next, next_child) = child.dirs.iter().next().expect("one child");
            name.push('/');
            name.push_str(next);
            child = next_child;
        }
        let path = join(parent, &name);
        let expanded = !collapsed.iter().any(|candidate| {
            path == *candidate
                || path
                    .strip_prefix(candidate)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        });
        rows.push(Row::Directory {
            path: path.clone(),
            name,
            depth,
            expanded,
        });
        if expanded {
            walk(child, &path, depth + 1, collapsed, rows);
        }
    }

    let mut files = node.files.clone();
    files.sort_by(|(a, _), (b, _)| name_cmp(a, b));
    rows.extend(
        files
            .into_iter()
            .map(|(_, index)| Row::File { index, depth }),
    );
}

fn join(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

fn name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Directory { name, depth, .. } => format!("{depth}:d:{name}"),
                Row::File { index, depth } => format!("{depth}:f:{index}"),
            })
            .collect()
    }

    #[test]
    fn compacts_single_directory_chains_and_sorts_dirs_first() {
        let tree = Tree::new([
            (0, "z.txt"),
            (1, "src/components/Button.rs"),
            (2, "src/components/input.rs"),
            (3, "Assets/logo.png"),
        ]);
        assert_eq!(
            labels(&tree.rows(&BTreeSet::new())),
            [
                "0:d:Assets",
                "1:f:3",
                "0:d:src/components",
                "1:f:1",
                "1:f:2",
                "0:f:0"
            ]
        );
    }

    #[test]
    fn collapsed_paths_hide_only_their_descendants() {
        let tree = Tree::new([(0, "src/a.rs"), (1, "tests/a.rs"), (2, "root.rs")]);
        let collapsed = BTreeSet::from(["src".to_string()]);
        assert_eq!(
            labels(&tree.rows(&collapsed)),
            ["0:d:src", "0:d:tests", "1:f:1", "0:f:2"]
        );
    }

    #[test]
    fn collapse_all_records_every_visible_directory() {
        let tree = Tree::new([(0, "src/ui/a.rs"), (1, "src/api/b.rs")]);
        let mut collapsed = BTreeSet::new();
        tree.collapse_all(&mut collapsed);
        assert_eq!(
            collapsed,
            BTreeSet::from(["src".into(), "src/api".into(), "src/ui".into()])
        );
        assert_eq!(labels(&tree.rows(&collapsed)), ["0:d:src"]);
    }
}
