//! Editor-style diff rendering: one line-number gutter with red/green change
//! bars, syntax-highlighted code over row tints, and a darker word-level tint
//! on changed segments. Input is plain `git diff` output (no ANSI).

use std::collections::HashMap;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::syntax::LineHighlighter;

/// Catppuccin Mocha layers, matching the preview syntax theme.
const DEL_BG: Color = Color::Rgb(0x45, 0x23, 0x2f);
const DEL_WORD_BG: Color = Color::Rgb(0x6e, 0x34, 0x46);
const ADD_BG: Color = Color::Rgb(0x1f, 0x3a, 0x2a);
const ADD_WORD_BG: Color = Color::Rgb(0x30, 0x55, 0x3f);
const DEL_MARK: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
const ADD_MARK: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
const GUTTER_FG: Color = Color::Rgb(0x7f, 0x84, 0x9c);
const FOLD_BG: Color = Color::Rgb(0x31, 0x32, 0x44);
const FOLD_FG: Color = Color::Rgb(0xa6, 0xad, 0xc8);

/// One parsed diff line, before rendering.
#[derive(Debug, PartialEq, Eq)]
enum Ev {
    /// Unchanged lines omitted before the next hunk.
    Fold(usize),
    /// Unchanged: (old line no, new line no, text).
    Ctx(usize, usize, String),
    Del(usize, String),
    Add(usize, String),
    /// Anything unparsed worth keeping ("Binary files … differ").
    Plain(String),
}

fn parse_events(diff: &str) -> Vec<Ev> {
    let mut evs = Vec::new();
    let mut old_no = 0usize;
    let mut new_no = 0usize;
    let mut seen_hunk = false;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git") {
            in_hunk = false;
            seen_hunk = false;
        }
        if (!in_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")))
            || line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity")
            || line.starts_with("rename ")
            || line.starts_with("copy ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with('\\')
        {
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            let mut next_old = None;
            let mut next_new = None;
            for tok in line.split_whitespace() {
                let (sign, rest) = match tok.split_at_checked(1) {
                    Some(pair) => pair,
                    None => continue,
                };
                let start = rest.split(',').next().unwrap_or("");
                if let Ok(n) = start.parse::<usize>() {
                    match sign {
                        "-" => next_old = Some(n),
                        "+" => next_new = Some(n),
                        _ => {}
                    }
                }
            }
            let (Some(next_old), Some(next_new)) = (next_old, next_new) else {
                continue;
            };
            let hidden_old = if seen_hunk {
                next_old.saturating_sub(old_no)
            } else {
                next_old.saturating_sub(1)
            };
            let hidden_new = if seen_hunk {
                next_new.saturating_sub(new_no)
            } else {
                next_new.saturating_sub(1)
            };
            let hidden = hidden_old.min(hidden_new);
            if hidden > 0 {
                evs.push(Ev::Fold(hidden));
            }
            old_no = next_old;
            new_no = next_new;
            seen_hunk = true;
            continue;
        }
        if let Some(t) = line.strip_prefix('-') {
            evs.push(Ev::Del(old_no, t.to_string()));
            old_no += 1;
        } else if let Some(t) = line.strip_prefix('+') {
            evs.push(Ev::Add(new_no, t.to_string()));
            new_no += 1;
        } else if let Some(t) = line.strip_prefix(' ') {
            evs.push(Ev::Ctx(old_no, new_no, t.to_string()));
            old_no += 1;
            new_no += 1;
        } else if line.is_empty() {
            // Some tools trim the leading space off blank context lines.
            evs.push(Ev::Ctx(old_no, new_no, String::new()));
            old_no += 1;
            new_no += 1;
        } else {
            evs.push(Ev::Plain(line.to_string()));
        }
    }
    evs
}

/// Char range (start, end) of the differing middle of a changed line,
/// keyed by event index — deletions paired 1:1 with the additions that
/// immediately follow them, VS Code style.
fn word_ranges(evs: &[Ev]) -> HashMap<usize, (usize, usize)> {
    let mut ranges = HashMap::new();
    let mut i = 0;
    while i < evs.len() {
        let del_start = i;
        while matches!(evs.get(i), Some(Ev::Del(..))) {
            i += 1;
        }
        let add_start = i;
        while matches!(evs.get(i), Some(Ev::Add(..))) {
            i += 1;
        }
        if del_start == add_start || add_start == i {
            if i == del_start {
                i += 1;
            }
            continue;
        }
        for k in 0..(add_start - del_start).min(i - add_start) {
            let (Some(Ev::Del(_, old)), Some(Ev::Add(_, new))) =
                (evs.get(del_start + k), evs.get(add_start + k))
            else {
                continue;
            };
            let old_chars: Vec<char> = old.chars().collect();
            let new_chars: Vec<char> = new.chars().collect();
            let mut prefix = 0;
            while prefix < old_chars.len()
                && prefix < new_chars.len()
                && old_chars[prefix] == new_chars[prefix]
            {
                prefix += 1;
            }
            let mut suffix = 0;
            while suffix < old_chars.len().saturating_sub(prefix)
                && suffix < new_chars.len().saturating_sub(prefix)
                && old_chars[old_chars.len() - 1 - suffix] == new_chars[new_chars.len() - 1 - suffix]
            {
                suffix += 1;
            }
            // Only worth tinting when the lines genuinely share material.
            if prefix + suffix == 0 {
                continue;
            }
            ranges.insert(del_start + k, (prefix, old_chars.len() - suffix));
            ranges.insert(add_start + k, (prefix, new_chars.len() - suffix));
        }
    }
    ranges
}

/// Re-slice spans so `range` (in chars) carries `bg` — the word-level tint.
fn overlay_bg(spans: Vec<Span<'static>>, range: (usize, usize), bg: Color) -> Vec<Span<'static>> {
    let (start, end) = range;
    if start >= end {
        return spans;
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        let len = chars.len();
        let (a, b) = (start.clamp(pos, pos + len), end.clamp(pos, pos + len));
        if a == b || a >= pos + len {
            out.push(span);
        } else {
            let style = span.style;
            let before: String = chars[..a - pos].iter().collect();
            let middle: String = chars[a - pos..b - pos].iter().collect();
            let after: String = chars[b - pos..].iter().collect();
            if !before.is_empty() {
                out.push(Span::styled(before, style));
            }
            out.push(Span::styled(middle, style.bg(bg)));
            if !after.is_empty() {
                out.push(Span::styled(after, style));
            }
        }
        pos += len;
    }
    out
}

/// Render a unified diff for `rel` (its extension picks the grammar) into
/// display lines: line number + change bar, tinted rows, highlighted code.
pub fn render(rel: &str, diff: &str) -> Vec<Line<'static>> {
    let evs = parse_events(diff);
    let ranges = word_ranges(&evs);

    let max_no = evs
        .iter()
        .map(|e| match e {
            Ev::Ctx(o, n, _) => (*o).max(*n),
            Ev::Del(o, _) => *o,
            Ev::Add(n, _) => *n,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let w = max_no.to_string().len().max(2);

    // Two stateful highlighters approximate the old and new file contexts,
    // so multi-line constructs mostly survive (the delta/bat trick).
    let mut old_hl = LineHighlighter::new(rel);
    let mut new_hl = LineHighlighter::new(rel);

    let gutter = |line: Option<usize>, mark: Option<Color>| {
        let line = line.map(|n| n.to_string()).unwrap_or_default();
        vec![
            Span::styled(format!("{line:>w$} "), Style::default().fg(GUTTER_FG)),
            match mark {
                Some(color) => Span::styled("▌ ", Style::default().fg(color)),
                None => Span::raw("  "),
            },
        ]
    };

    let mut lines = Vec::new();
    for (idx, ev) in evs.iter().enumerate() {
        match ev {
            Ev::Fold(hidden) => lines.push(
                Line::from(Span::styled(
                    format!("{}⋯  {hidden} unmodified lines", " ".repeat(w + 2)),
                    Style::default().fg(FOLD_FG),
                ))
                .style(Style::default().bg(FOLD_BG)),
            ),
            Ev::Plain(t) => lines.push(Line::from(Span::styled(
                t.clone(),
                Style::default().dim(),
            ))),
            Ev::Ctx(_o, n, t) => {
                old_hl.line(t);
                let spans = new_hl.line(t);
                let mut all = gutter(Some(*n), None);
                all.extend(spans);
                lines.push(Line::from(all));
            }
            Ev::Del(o, t) => {
                let mut spans = old_hl.line(t);
                if let Some(&range) = ranges.get(&idx) {
                    spans = overlay_bg(spans, range, DEL_WORD_BG);
                }
                let mut all = gutter(Some(*o), Some(DEL_MARK));
                all.extend(spans);
                lines.push(Line::from(all).style(Style::default().bg(DEL_BG)));
            }
            Ev::Add(n, t) => {
                let mut spans = new_hl.line(t);
                if let Some(&range) = ranges.get(&idx) {
                    spans = overlay_bg(spans, range, ADD_WORD_BG);
                }
                let mut all = gutter(Some(*n), Some(ADD_MARK));
                all.extend(spans);
                lines.push(Line::from(all).style(Style::default().bg(ADD_BG)));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: no `\` line continuations — they would eat the leading space
    // that marks context lines.
    const DIFF: &str = concat!(
        "diff --git a/src/app.ts b/src/app.ts\n",
        "index 111..222 100644\n",
        "--- a/src/app.ts\n",
        "+++ b/src/app.ts\n",
        "@@ -1,4 +1,5 @@\n",
        " import { search } from \"./search\";\n",
        "-console.log(\"scm-playground up\");\n",
        "+console.log(\"scm oh yeah\");\n",
        "+// TODO: debounce input\n",
        " search(\"hello\");\n",
    );

    #[test]
    fn diff_parses_gutters_tints_and_word_ranges() {
        let lines = render("app.ts", DIFF);
        assert_eq!(lines.len(), 5);
        // Context row: one number, no tint.
        assert!(lines[0].to_string().starts_with(" 1   "));
        assert_eq!(lines[0].style.bg, None);
        // Deletion and addition use the same clean bar gutter; color carries
        // which side they belong to.
        assert!(lines[1].to_string().starts_with(" 2 ▌ "));
        assert_eq!(lines[1].style.bg, Some(DEL_BG));
        assert!(lines[2].to_string().starts_with(" 2 ▌ "));
        assert_eq!(lines[2].style.bg, Some(ADD_BG));
        // The paired del/add carry a darker word-level tint on the middle.
        let word_tinted = lines[1]
            .spans
            .iter()
            .any(|s| s.style.bg == Some(DEL_WORD_BG));
        assert!(word_tinted, "expected word-level tint on the deletion");
        // The unpaired trailing addition has no word tint.
        let plain_add = lines[3]
            .spans
            .iter()
            .all(|s| s.style.bg != Some(ADD_WORD_BG));
        assert!(plain_add);
    }

    #[test]
    fn deleted_line_starting_with_dashes_does_not_desync_gutters() {
        // A deleted SQL/Lua-style "-- comment" line looks exactly like a
        // "--- a/file" header if the parser isn't hunk-aware.
        let diff = concat!(
            "diff --git a/q.sql b/q.sql\n",
            "index 111..222 100644\n",
            "--- a/q.sql\n",
            "+++ b/q.sql\n",
            "@@ -1,3 +1,2 @@\n",
            " SELECT 1;\n",
            "--- trailing comment\n",
            " SELECT 2;\n",
        );
        let lines = render("q.sql", diff);
        assert_eq!(lines.len(), 3, "the deleted comment line must render");
        assert!(lines[0].to_string().starts_with(" 1   "));
        assert!(lines[1].to_string().contains("-- trailing comment"));
        // Old-side numbering must not skip: line 3 is old line 3, not 2.
        assert!(matches!(&parse_events(diff)[2], Ev::Ctx(3, 2, _)));
    }

    #[test]
    fn hunk_boundaries_and_binary_lines_survive() {
        let two_hunks = "@@ -1,1 +1,1 @@\n ctx\n@@ -9,1 +9,1 @@\n ctx2\n";
        let lines = render("x.rs", two_hunks);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].to_string().contains("⋯  7 unmodified lines"));
        let bin = render("x.bin", "Binary files a/x.bin and b/x.bin differ\n");
        assert_eq!(bin.len(), 1);
    }

    #[test]
    fn leading_omitted_context_reports_the_fold_size() {
        let lines = render("x.txt", "@@ -10,2 +10,2 @@\n-old\n+new\n same\n");
        assert_eq!(lines[0].to_string(), "    ⋯  9 unmodified lines");
    }
}
