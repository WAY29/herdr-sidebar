//! Git plumbing: repo discovery, `status --porcelain -z` parsing, and the
//! stage / unstage / commit operations, all via the `git` CLI (no libgit2).
//! Parsing is pure and unit-tested; commands run with the repo toplevel as cwd
//! so the repo-relative paths porcelain reports resolve even when the pane's
//! cwd is a subdirectory.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub added: Option<usize>,
    pub deleted: Option<usize>,
}

/// One file in the staged or unstaged list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Repo-relative path (the new path, for renames), `/`-separated as git reports it.
    pub path: String,
    /// Rename/copy source, when there is one — unstaging a rename must reset both.
    pub orig: Option<String>,
    /// The VS Code-style status letter to display: M, A, D, R, C, U (untracked),
    /// or `!` for merge conflicts.
    pub letter: char,
    /// Text-line counts. `None` means Git reported binary/unknown.
    pub stat: DiffStat,
}

/// A historical file plus the immutable revision pair its diff compares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefFile {
    pub entry: FileEntry,
    pub old_spec: String,
    pub new_spec: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHistoryEntry {
    pub line: String,
    pub file: RefFile,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub branch: String,
    pub staged: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
    /// Commits ahead of / behind the upstream, from the porcelain `##` header.
    pub ahead: usize,
    pub behind: usize,
    /// The branch has an upstream at all (the header carries `...remote`).
    pub has_upstream: bool,
}

#[derive(Clone)]
pub struct Git {
    root: PathBuf,
}

impl Git {
    /// Locate the repository containing `dir`; Err with git's message when there
    /// is none (or git itself is missing).
    pub fn discover(dir: &Path) -> Result<Git, String> {
        let out = run_in(dir, &["rev-parse", "--show-toplevel"])?;
        let root = out.trim();
        if root.is_empty() {
            return Err("not inside a git repository".to_string());
        }
        Ok(Git { root: PathBuf::from(root) })
    }

    /// All repositories visible from `dir`, VS Code style: the repository
    /// containing `dir` (if any) plus child repositories up to two directory
    /// levels down (a `.git` dir, or a `.git` FILE — worktrees/submodules).
    /// Deduped by root; the containing repo sorts first, children by path.
    pub fn discover_all(dir: &Path) -> Vec<Git> {
        let mut repos: Vec<Git> = Vec::new();
        let mut push = |git: Git| {
            if !repos.iter().any(|r| r.root == git.root) {
                repos.push(git);
            }
        };
        if let Ok(git) = Git::discover(dir) {
            push(git);
        }
        for child in child_dirs(dir, 2) {
            if child.join(".git").exists()
                && let Ok(git) = Git::discover(&child)
            {
                push(git);
            }
        }
        repos
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Display name for repo headers: the root directory's name.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// VS Code's Sync Changes: pull (rebase, autostash) then push. Returns a
    /// short human summary; the caller runs this on a background thread.
    pub fn sync(&self) -> Result<String, String> {
        run_in(&self.root, &["pull", "--rebase", "--autostash"])?;
        run_in(&self.root, &["push"])?;
        Ok("synced with remote".to_string())
    }

    pub fn status(&self) -> Result<Status, String> {
        let out = run_in(
            &self.root,
            &["status", "--porcelain", "-z", "--branch", "--untracked-files=all"],
        )?;
        let mut status = parse_status(&out);
        if let Ok(raw) = run_in(
            &self.root,
            &["diff", "--numstat", "-z", "-M", "--ignore-submodules=all", "--no-ext-diff"],
        ) {
            attach_stats(&mut status.unstaged, &parse_numstat(&raw));
        }
        if let Ok(raw) = run_in(
            &self.root,
            &[
                "diff",
                "--cached",
                "--numstat",
                "-z",
                "-M",
                "--ignore-submodules=all",
                "--no-ext-diff",
            ],
        ) {
            attach_stats(&mut status.staged, &parse_numstat(&raw));
        }
        let mut untracked_budget = 8 * 1024 * 1024;
        for entry in &mut status.unstaged {
            if entry.letter == 'U' {
                entry.stat = DiffStat {
                    added: text_line_count(&self.root.join(&entry.path), &mut untracked_budget),
                    deleted: Some(0),
                };
            }
        }
        Ok(status)
    }

    /// Stage one entry: `add -A` records modifications, additions, and deletions alike.
    pub fn stage(&self, entry: &FileEntry) -> Result<(), String> {
        let paths = entry_paths(std::slice::from_ref(entry));
        self.stage_paths(&paths)
    }

    pub fn stage_paths(&self, paths: &[&str]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["add", "-A", "--"];
        args.extend(paths.iter().copied());
        run_in(&self.root, &args).map(drop)
    }

    pub fn stage_all(&self) -> Result<(), String> {
        run_in(&self.root, &["add", "-A"]).map(drop)
    }

    /// Whether HEAD resolves to a real commit — false only on an unborn
    /// branch (a repo with no commits yet), where `git reset` has nothing to
    /// reset against.
    fn has_head(&self) -> bool {
        run_in(&self.root, &["rev-parse", "--verify", "HEAD"]).is_ok()
    }

    /// Unstage one entry. `reset` needs a HEAD to reset against; on an unborn
    /// branch (no commits yet) fall back to dropping the path from the index.
    /// The fallback is destructive on a repo WITH a HEAD (`rm --cached` stages
    /// a deletion instead of unstaging), so it only runs for the unborn-branch
    /// case — any other `reset` failure is propagated instead of swallowed.
    pub fn unstage(&self, entry: &FileEntry) -> Result<(), String> {
        let paths = entry_paths(std::slice::from_ref(entry));
        self.unstage_paths(&paths)
    }

    pub fn unstage_paths(&self, paths: &[&str]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["reset", "-q", "--"];
        args.extend(paths.iter().copied());
        match run_in(&self.root, &args) {
            Ok(_) => Ok(()),
            Err(e) if self.has_head() => Err(e),
            Err(_) => {
                let mut args = vec!["rm", "--cached", "-r", "-q", "--"];
                args.extend(paths.iter().copied());
                run_in(&self.root, &args).map(drop)
            }
        }
    }

    pub fn unstage_all(&self) -> Result<(), String> {
        match run_in(&self.root, &["reset", "-q"]) {
            Ok(_) => Ok(()),
            Err(e) if self.has_head() => Err(e),
            Err(_) => run_in(&self.root, &["rm", "--cached", "-r", "-q", "--", "."]).map(drop),
        }
    }

    /// Commit the staged changes; returns git's summary line ("[branch abc1234] …").
    pub fn commit(&self, message: &str) -> Result<String, String> {
        let out = run_in(&self.root, &["commit", "-m", message])?;
        Ok(out.lines().next().unwrap_or("committed").to_string())
    }

    /// Throw away a file's working-tree changes: untracked files are deleted,
    /// tracked ones restored from HEAD (the caller confirms first).
    pub fn discard(&self, entry: &FileEntry) -> Result<(), String> {
        if entry.letter == 'U' {
            return run_in(&self.root, &["clean", "-fd", "--", &entry.path]).map(drop);
        }
        run_in(&self.root, &["checkout", "--", &entry.path]).map(drop)
    }

    /// The diff a commit-message suggestion should describe: the staged diff
    /// when something is staged (that is what would be committed), else the
    /// working-tree diff. Untracked files only appear as names, so they ride
    /// along in the returned path list either way.
    pub fn diff_for_message(&self) -> Result<(String, Vec<String>), String> {
        let staged = run_in(&self.root, &["diff", "--cached", "--stat", "--patch"])?;
        let (diff, names_args): (String, &[&str]) = if staged.trim().is_empty() {
            let unstaged = run_in(&self.root, &["diff", "--stat", "--patch"])?;
            (unstaged, &["diff", "--name-only"])
        } else {
            (staged, &["diff", "--cached", "--name-only"])
        };
        let mut files: Vec<String> = run_in(&self.root, names_args)?
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect();
        if files.is_empty() {
            // Nothing tracked changed: describe the untracked files instead.
            files = run_in(&self.root, &["ls-files", "--others", "--exclude-standard"])?
                .lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect();
        }
        Ok((diff, files))
    }

    // ---- Drawer queries (display-only lists, VS Code Git-Graph style) ----

    pub fn graph(&self, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        lines(run_in(&self.root, &["log", "--graph", "--oneline", "--decorate=short", &n])?)
    }

    pub fn commits(&self, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        lines(run_in(
            &self.root,
            &["log", "--oneline", "--decorate=short", "--date=short", &n],
        )?)
    }

    pub fn file_history(&self, path: &str, limit: usize) -> Result<Vec<FileHistoryEntry>, String> {
        let n = format!("-{limit}");
        let out = run_in(
            &self.root,
            &[
                "log",
                &n,
                "--follow",
                "-M",
                "-z",
                "--name-status",
                "--format=%x1e%H%x00%P%x00%s%x00",
                "--",
                path,
            ],
        )?;
        Ok(parse_file_history(&out))
    }

    /// Files introduced by a commit-ish ref relative to its first parent.
    pub fn ref_files(&self, spec: &str) -> Result<Vec<RefFile>, String> {
        let peeled = format!("{spec}^{{commit}}");
        let commit = run_in(&self.root, &["rev-parse", "--verify", &peeled])?;
        let commit = commit.trim();
        let old = self.first_parent_or_empty(commit)?;
        self.range_files(&old, commit)
    }

    /// A stash's final tracked snapshot plus any third-parent untracked tree.
    pub fn stash_files(&self, spec: &str) -> Result<Vec<RefFile>, String> {
        let commit = run_in(&self.root, &["rev-parse", "--verify", spec])?;
        let commit = commit.trim();
        let base = self.first_parent_or_empty(commit)?;
        let mut files = self.range_files(&base, commit)?;
        let third = format!("{commit}^3");
        if let Ok(untracked) = run_in(&self.root, &["rev-parse", "--verify", &third]) {
            let untracked = untracked.trim();
            let seen: HashSet<String> = files.iter().map(|file| file.entry.path.clone()).collect();
            files.extend(
                self.range_files(EMPTY_TREE, untracked)?
                    .into_iter()
                    .filter(|file| !seen.contains(&file.entry.path)),
            );
        }
        Ok(files)
    }

    fn first_parent_or_empty(&self, commit: &str) -> Result<String, String> {
        let out = run_in(&self.root, &["rev-list", "--parents", "-n", "1", commit])?;
        Ok(out.split_whitespace().nth(1).unwrap_or(EMPTY_TREE).to_string())
    }

    fn range_files(&self, old: &str, new: &str) -> Result<Vec<RefFile>, String> {
        let names = run_in(
            &self.root,
            &[
                "diff",
                "--name-status",
                "-z",
                "-M",
                "--ignore-submodules=none",
                "--no-ext-diff",
                old,
                new,
            ],
        )?;
        let stats = run_in(
            &self.root,
            &[
                "diff",
                "--numstat",
                "-z",
                "-M",
                "--ignore-submodules=all",
                "--no-ext-diff",
                old,
                new,
            ],
        )
        .map(|raw| parse_numstat(&raw))
        .unwrap_or_default();
        Ok(parse_name_status(&names)
            .into_iter()
            .map(|mut entry| {
                entry.stat = stats.get(&entry.path).copied().unwrap_or_default();
                RefFile {
                    entry,
                    old_spec: old.to_string(),
                    new_spec: new.to_string(),
                }
            })
            .collect())
    }

    /// Local + remote branches, the current one first and starred.
    pub fn branches(&self) -> Result<Vec<String>, String> {
        lines(run_in(
            &self.root,
            &["branch", "-a", "--sort=-committerdate", "--format=%(HEAD) %(refname:short)"],
        )?)
    }

    pub fn remotes(&self) -> Result<Vec<String>, String> {
        let out = run_in(&self.root, &["remote", "-v"])?;
        // `remote -v` lists fetch and push separately; one line per remote reads better.
        let mut seen = Vec::new();
        for line in out.lines() {
            if let Some(rest) = line.strip_suffix(" (fetch)") {
                seen.push(rest.replace('\t', "  "));
            }
        }
        Ok(seen)
    }

    pub fn stashes(&self) -> Result<Vec<String>, String> {
        lines(run_in(&self.root, &["stash", "list"])?)
    }

    pub fn tags(&self) -> Result<Vec<String>, String> {
        lines(run_in(&self.root, &["tag", "--sort=-creatordate"])?)
    }

    /// One line per worktree (`git worktree list`): path, short head,
    /// [branch] — the primary checkout first.
    pub fn worktrees(&self) -> Result<Vec<String>, String> {
        lines(run_in(&self.root, &["worktree", "list"])?)
    }

    /// Run an arbitrary git command in this repo — the escape hatch the
    /// drawer context menus use (checkout / merge / cherry-pick / …).
    pub fn raw(&self, args: &[&str]) -> Result<String, String> {
        run_in(&self.root, args)
    }
}

fn lines(out: String) -> Result<Vec<String>, String> {
    Ok(out.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
}

fn run_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-c")
        .arg("color.ui=false")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .to_string())
}

fn entry_paths(entries: &[FileEntry]) -> Vec<&str> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for entry in entries {
        for path in std::iter::once(entry.path.as_str()).chain(entry.orig.as_deref()) {
            if seen.insert(path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn attach_stats(entries: &mut [FileEntry], stats: &HashMap<String, DiffStat>) {
    for entry in entries {
        entry.stat = stats.get(&entry.path).copied().unwrap_or_default();
    }
}

/// Git does not include untracked files in numstat. Keep the synchronous
/// refresh bounded; large or NUL-containing files simply omit the counters.
fn text_line_count(path: &Path, remaining_bytes: &mut u64) -> Option<usize> {
    const MAX_BYTES: u64 = 1024 * 1024;
    // ponytail: bounded per-file and per-refresh reads; cache when exhaustive
    // untracked-file stats matter more than SCM refresh latency.
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() > MAX_BYTES
        || metadata.len() > *remaining_bytes
    {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    *remaining_bytes = remaining_bytes.saturating_sub(metadata.len());
    if bytes.contains(&0) {
        return None;
    }
    Some(bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n')))
}

pub fn parse_numstat(raw: &str) -> HashMap<String, DiffStat> {
    let mut stats = HashMap::new();
    let mut parts = raw.split('\0');
    while let Some(head) = parts.next() {
        let head = head.trim_start_matches('\n');
        if head.is_empty() {
            continue;
        }
        let mut columns = head.splitn(3, '\t');
        let Some(added) = columns.next() else { continue };
        let Some(deleted) = columns.next() else { continue };
        let Some(path) = columns.next() else { continue };
        let path = if path.is_empty() {
            let _orig = parts.next();
            parts.next().unwrap_or("")
        } else {
            path
        };
        if path.is_empty() {
            continue;
        }
        stats.insert(
            path.to_string(),
            DiffStat {
                added: parse_count(added),
                deleted: parse_count(deleted),
            },
        );
    }
    stats
}

fn parse_count(value: &str) -> Option<usize> {
    if value == "-" { None } else { value.parse().ok() }
}

fn parse_name_status(raw: &str) -> Vec<FileEntry> {
    let mut files = Vec::new();
    let mut parts = raw.split('\0');
    while let Some(status) = parts.next() {
        let status = status.trim_start_matches('\n');
        if status.is_empty() {
            continue;
        }
        let letter = display_letter(status.chars().next().unwrap_or('M'));
        let (orig, path) = if matches!(letter, 'R' | 'C') {
            (parts.next().map(str::to_string), parts.next())
        } else {
            (None, parts.next())
        };
        let Some(path) = path.filter(|path| !path.is_empty()) else { continue };
        files.push(FileEntry {
            path: path.to_string(),
            orig,
            letter,
            stat: DiffStat::default(),
        });
    }
    files
}

fn parse_file_history(raw: &str) -> Vec<FileHistoryEntry> {
    let mut history = Vec::new();
    for record in raw.split('\x1e').skip(1) {
        let mut fields = record.split('\0');
        let Some(hash) = fields.next().filter(|value| !value.is_empty()) else { continue };
        let parents = fields.next().unwrap_or("");
        let subject = fields.next().unwrap_or("");
        let mut tail = fields.map(|field| field.trim_start_matches('\n')).filter(|field| !field.is_empty());
        let Some(status) = tail.next() else { continue };
        let letter = display_letter(status.chars().next().unwrap_or('M'));
        let (orig, path) = if matches!(letter, 'R' | 'C') {
            (tail.next().map(str::to_string), tail.next())
        } else {
            (None, tail.next())
        };
        let Some(path) = path else { continue };
        let short = &hash[..hash.len().min(7)];
        history.push(FileHistoryEntry {
            line: format!("{short} {subject}"),
            file: RefFile {
                entry: FileEntry {
                    path: path.to_string(),
                    orig,
                    letter,
                    stat: DiffStat::default(),
                },
                old_spec: parents.split_whitespace().next().unwrap_or(EMPTY_TREE).to_string(),
                new_spec: hash.to_string(),
            },
        });
    }
    history
}

/// Parse `git status --porcelain -z --branch` output. Entries are NUL-separated
/// `XY path`; a rename/copy is followed by a second NUL-separated field holding
/// the source path. X is the index (staged) state, Y the worktree state.
pub fn parse_status(raw: &str) -> Status {
    let mut status = Status::default();
    let mut parts = raw.split('\0');
    while let Some(entry) = parts.next() {
        if entry.is_empty() {
            continue;
        }
        if let Some(header) = entry.strip_prefix("## ") {
            status.branch = parse_branch(header);
            (status.ahead, status.behind) = parse_ahead_behind(header);
            status.has_upstream = header.contains("...");
            continue;
        }
        let Some((xy, path)) = split_entry(entry) else {
            continue;
        };
        let (x, y) = xy;
        let orig = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            parts.next().filter(|s| !s.is_empty()).map(str::to_string)
        } else {
            None
        };
        let path = path.to_string();
        if x == '?' && y == '?' {
            status.unstaged.push(FileEntry {
                path,
                orig: None,
                letter: 'U',
                stat: DiffStat::default(),
            });
            continue;
        }
        if x == '!' {
            continue; // ignored file
        }
        if is_conflict(x, y) {
            status.unstaged.push(FileEntry {
                path,
                orig,
                letter: '!',
                stat: DiffStat::default(),
            });
            continue;
        }
        if x != ' ' {
            status.staged.push(FileEntry {
                path: path.clone(),
                orig: orig.clone(),
                letter: display_letter(x),
                stat: DiffStat::default(),
            });
        }
        if y != ' ' {
            status.unstaged.push(FileEntry {
                path,
                orig,
                letter: display_letter(y),
                stat: DiffStat::default(),
            });
        }
    }
    status
}

/// `("XY", path)` from one porcelain entry; the XY columns are always ASCII.
fn split_entry(entry: &str) -> Option<((char, char), &str)> {
    let bytes = entry.as_bytes();
    if bytes.len() < 4 || bytes[2] != b' ' {
        return None;
    }
    Some(((bytes[0] as char, bytes[1] as char), &entry[3..]))
}

fn is_conflict(x: char, y: char) -> bool {
    matches!(
        (x, y),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
    )
}

/// Type changes (T) read as plain modifications, matching VS Code.
fn display_letter(c: char) -> char {
    if c == 'T' { 'M' } else { c }
}

/// Branch from the `## …` header: `main...origin/main [ahead 1]`, bare `main`,
/// `No commits yet on main`, or `HEAD (no branch)` when detached.
fn parse_branch(header: &str) -> String {
    let head = header.split("...").next().unwrap_or(header);
    head.strip_prefix("No commits yet on ")
        .unwrap_or(head)
        .to_string()
}

/// `(ahead, behind)` from the header's `[ahead 1, behind 2]` suffix (either
/// half may be absent; `[gone]` and no-bracket headers give zeros).
fn parse_ahead_behind(header: &str) -> (usize, usize) {
    let Some(bracket) = header.rsplit_once('[').map(|(_, b)| b.trim_end_matches(']')) else {
        return (0, 0);
    };
    let count_after = |tag: &str| {
        bracket
            .split(',')
            .map(str::trim)
            .find_map(|part| part.strip_prefix(tag))
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0)
    };
    (count_after("ahead "), count_after("behind "))
}

/// Directories under `dir`, `depth` levels deep, skipping build/VCS internals
/// (the repo scan visits each).
fn child_dirs(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if depth == 0 {
        return out;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), ".git" | "target" | "node_modules" | ".claude") {
            continue;
        }
        out.push(path.clone());
        out.extend(child_dirs(&path, depth - 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, letter: char, orig: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            orig: orig.map(str::to_string),
            letter,
            stat: DiffStat::default(),
        }
    }

    #[test]
    fn parses_numstat_regular_rename_and_binary_rows() {
        let stats = parse_numstat("3\t2\tsrc/a.rs\0\n1\t0\t\0old.rs\0new.rs\0-\t-\tlogo.png\0");
        assert_eq!(stats["src/a.rs"], DiffStat { added: Some(3), deleted: Some(2) });
        assert_eq!(stats["new.rs"], DiffStat { added: Some(1), deleted: Some(0) });
        assert_eq!(stats["logo.png"], DiffStat::default());
    }

    #[test]
    fn parses_file_history_with_rename_paths_and_first_parent() {
        let raw = "\x1eabcdef0123456789\0parent second\0move file\0\0\nR100\0old.rs\0src/new.rs\0";
        let history = parse_file_history(raw);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].line, "abcdef0 move file");
        assert_eq!(history[0].file.entry.path, "src/new.rs");
        assert_eq!(history[0].file.entry.orig.as_deref(), Some("old.rs"));
        assert_eq!(history[0].file.old_spec, "parent");
    }

    #[test]
    fn historical_files_use_real_git_rename_stats_and_follow_paths() {
        let root = std::env::temp_dir().join(format!(
            "aa-git-history-files-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        run_in(&root, &["init", "-q"]).unwrap();
        run_in(&root, &["config", "user.name", "Test"]).unwrap();
        run_in(&root, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(root.join("src/old.txt"), "one\ntwo\n").unwrap();
        run_in(&root, &["add", "."]).unwrap();
        run_in(&root, &["commit", "-qm", "first"]).unwrap();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        run_in(&root, &["mv", "src/old.txt", "lib/new.txt"]).unwrap();
        std::fs::write(root.join("lib/new.txt"), "one\ntwo\nthree\n").unwrap();
        run_in(&root, &["commit", "-qam", "rename"]).unwrap();

        let git = Git { root: root.clone() };
        let files = git.ref_files("HEAD").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].entry.path, "lib/new.txt");
        assert_eq!(files[0].entry.orig.as_deref(), Some("src/old.txt"));
        assert_eq!(files[0].entry.letter, 'R');
        assert_eq!(files[0].entry.stat, DiffStat { added: Some(1), deleted: Some(0) });

        let history = git.file_history("lib/new.txt", 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].file.entry.path, "lib/new.txt");
        assert_eq!(history[0].file.entry.orig.as_deref(), Some("src/old.txt"));
        assert_eq!(history[1].file.entry.path, "src/old.txt");
        assert_eq!(history[1].file.old_spec, EMPTY_TREE);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stash_files_include_tracked_and_saved_untracked_snapshots() {
        let root = std::env::temp_dir().join(format!(
            "aa-git-stash-files-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        run_in(&root, &["init", "-q"]).unwrap();
        run_in(&root, &["config", "user.name", "Test"]).unwrap();
        run_in(&root, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(root.join("tracked.txt"), "one\n").unwrap();
        run_in(&root, &["add", "."]).unwrap();
        run_in(&root, &["commit", "-qm", "base"]).unwrap();
        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\nfile\n").unwrap();
        run_in(&root, &["stash", "push", "-u", "-m", "test"]).unwrap();

        let files = Git { root: root.clone() }.stash_files("stash@{0}").unwrap();
        let tracked = files.iter().find(|file| file.entry.path == "tracked.txt").unwrap();
        assert_eq!(tracked.entry.stat, DiffStat { added: Some(1), deleted: Some(0) });
        let untracked = files.iter().find(|file| file.entry.path == "new.txt").unwrap();
        assert_eq!(untracked.entry.stat, DiffStat { added: Some(2), deleted: Some(0) });
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_branch_variants() {
        assert_eq!(parse_status("## main...origin/main [ahead 1]\0").branch, "main");
        assert_eq!(parse_status("## git-panel\0").branch, "git-panel");
        assert_eq!(parse_status("## No commits yet on trunk\0").branch, "trunk");
        assert_eq!(parse_status("## HEAD (no branch)\0").branch, "HEAD (no branch)");
    }

    #[test]
    fn parses_ahead_behind_and_upstream() {
        let s = parse_status("## main...origin/main [ahead 3, behind 2]\0");
        assert_eq!((s.ahead, s.behind, s.has_upstream), (3, 2, true));
        let s = parse_status("## main...origin/main [behind 4]\0");
        assert_eq!((s.ahead, s.behind, s.has_upstream), (0, 4, true));
        let s = parse_status("## main...origin/main\0");
        assert_eq!((s.ahead, s.behind, s.has_upstream), (0, 0, true));
        let s = parse_status("## local-only\0");
        assert_eq!((s.ahead, s.behind, s.has_upstream), (0, 0, false));
        let s = parse_status("## main...origin/main [gone]\0");
        assert_eq!((s.ahead, s.behind), (0, 0));
    }

    #[test]
    fn discover_all_finds_child_repos_and_dedupes() {
        let base = std::env::temp_dir().join(format!("aa-git-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // base is NOT a repo; two children are (one nested two levels down),
        // and a `.git`-less child is ignored.
        for sub in ["a", "group/b", "plain"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        for repo in ["a", "group/b"] {
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(base.join(repo))
                .output()
                .unwrap();
        }
        let repos = Git::discover_all(&base);
        let mut names: Vec<String> = repos.iter().map(|r| r.name()).collect();
        names.sort();
        assert_eq!(names, ["a", "b"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A fresh repo with one commit on `main`, so HEAD resolves.
    fn repo_with_head(name: &str) -> Git {
        let root = std::env::temp_dir().join(format!("aa-git-unstage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            &["init", "-q"][..],
            &["-c", "user.email=t@t.dev", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"][..],
        ] {
            std::process::Command::new("git").args(args).current_dir(&root).output().unwrap();
        }
        Git { root }
    }

    #[test]
    fn directory_pathspec_stages_and_unstages_all_nested_changes() {
        let git = repo_with_head("directory-pathspec");
        let dir = git.root.join("src/nested");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one.txt"), "base\n").unwrap();
        std::fs::write(dir.join("two.txt"), "base\n").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        )
        .unwrap();

        std::fs::write(dir.join("one.txt"), "changed\n").unwrap();
        std::fs::remove_file(dir.join("two.txt")).unwrap();
        std::fs::write(dir.join("three.txt"), "new\n").unwrap();

        git.stage_paths(&["src"]).unwrap();
        let mut staged = git.status().unwrap().staged;
        staged.sort_by(|left, right| left.path.cmp(&right.path));
        assert_eq!(
            staged.iter().map(|entry| (entry.path.as_str(), entry.letter)).collect::<Vec<_>>(),
            [("src/nested/one.txt", 'M'), ("src/nested/three.txt", 'A'), ("src/nested/two.txt", 'D')]
        );

        git.unstage_paths(&["src"]).unwrap();
        let status = git.status().unwrap();
        assert!(status.staged.is_empty());
        let mut unstaged = status.unstaged;
        unstaged.sort_by(|left, right| left.path.cmp(&right.path));
        assert_eq!(
            unstaged.iter().map(|entry| (entry.path.as_str(), entry.letter)).collect::<Vec<_>>(),
            [("src/nested/one.txt", 'M'), ("src/nested/three.txt", 'U'), ("src/nested/two.txt", 'D')]
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn unstage_all_propagates_reset_failure_instead_of_destructive_fallback_when_head_exists() {
        let git = repo_with_head("unstage-all");
        std::fs::write(git.root.join("file.txt"), "v1").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        // Force `git reset` to fail deterministically without touching HEAD.
        std::fs::write(git.root.join(".git/index.lock"), "").unwrap();
        let result = git.unstage_all();
        std::fs::remove_file(git.root.join(".git/index.lock")).unwrap();
        assert!(result.is_err(), "a real reset failure must not report success");
        let status = git.status().unwrap();
        assert_eq!(status.staged.len(), 1, "file must still be staged, untouched");
        assert_eq!(status.staged[0].letter, 'A', "must still be a staged add, not a staged deletion");
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn unstage_all_falls_back_on_a_genuinely_unborn_branch() {
        let root = std::env::temp_dir()
            .join(format!("aa-git-unstage-unborn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::process::Command::new("git").args(["init", "-q"]).current_dir(&root).output().unwrap();
        std::fs::write(root.join("file.txt"), "v1").unwrap();
        let git = Git { root: root.clone() };
        run_in(&git.root, &["add", "-A"]).unwrap();
        // No commits yet: `git reset` has no HEAD to reset against.
        assert!(!git.has_head());
        assert!(git.unstage_all().is_ok());
        let status = git.status().unwrap();
        assert_eq!(status.staged, vec![]);
        let mut expected = entry("file.txt", 'U', None);
        expected.stat = DiffStat { added: Some(1), deleted: Some(0) };
        assert_eq!(status.unstaged, vec![expected]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn splits_staged_and_unstaged_sides() {
        let s = parse_status("## main\0MM src/app.rs\0A  new.rs\0 D gone.rs\0");
        assert_eq!(
            s.staged,
            vec![entry("src/app.rs", 'M', None), entry("new.rs", 'A', None)]
        );
        assert_eq!(
            s.unstaged,
            vec![entry("src/app.rs", 'M', None), entry("gone.rs", 'D', None)]
        );
    }

    #[test]
    fn untracked_shows_as_u() {
        let s = parse_status("?? docs/notes.md\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(s.unstaged, vec![entry("docs/notes.md", 'U', None)]);
    }

    #[test]
    fn rename_consumes_the_source_field() {
        let s = parse_status("R  new_name.rs\0old_name.rs\0?? after.txt\0");
        assert_eq!(s.staged, vec![entry("new_name.rs", 'R', Some("old_name.rs"))]);
        assert_eq!(s.unstaged, vec![entry("after.txt", 'U', None)]);
    }

    #[test]
    fn type_change_reads_as_modified() {
        let s = parse_status("T  link.sh\0 T other.sh\0");
        assert_eq!(s.staged, vec![entry("link.sh", 'M', None)]);
        assert_eq!(s.unstaged, vec![entry("other.sh", 'M', None)]);
    }

    #[test]
    fn conflicts_land_unstaged_with_bang() {
        let s = parse_status("UU merge.rs\0AA both.rs\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(
            s.unstaged,
            vec![entry("merge.rs", '!', None), entry("both.rs", '!', None)]
        );
    }

    #[test]
    fn garbage_and_ignored_entries_are_skipped() {
        let s = parse_status("!! target\0x\0\0 M ok.rs\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(s.unstaged, vec![entry("ok.rs", 'M', None)]);
    }

    #[test]
    fn paths_with_spaces_survive() {
        let s = parse_status("M  my docs/read me.md\0");
        assert_eq!(s.staged, vec![entry("my docs/read me.md", 'M', None)]);
    }
}
