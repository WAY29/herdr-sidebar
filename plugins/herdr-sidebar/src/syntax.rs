//! Syntax highlighting for the file preview: syntect with bat's extended
//! grammar set via `two-face` (syntect's own defaults lack TypeScript, TOML,
//! Dockerfile, …), on the pure-Rust `regex-fancy` engine — no oniguruma C
//! build on Windows. Foreground colors only: the terminal keeps its own
//! background, and unknown file types fall back to plain lines.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SyntectColor, FontStyle};
use syntect::parsing::{SyntaxDefinition, SyntaxReference, SyntaxSet, SyntaxSetBuilder};
use syntect::util::LinesWithEndings;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

pub type DiffTheme = EmbeddedThemeName;
pub const DEFAULT_DIFF_THEME: DiffTheme = EmbeddedThemeName::CatppuccinMocha;

#[derive(Clone, Copy)]
pub struct PreviewHighlightStyles {
    pub selection: Style,
    pub comment: Style,
    pub comment_border: Color,
}

pub fn diff_themes() -> &'static [DiffTheme] {
    EmbeddedLazyThemeSet::theme_names()
}

pub fn diff_theme_from_name(name: &str) -> Option<DiffTheme> {
    diff_themes()
        .iter()
        .copied()
        .find(|theme| theme.as_name() == name)
}

pub fn diff_theme_index(theme: DiffTheme) -> usize {
    diff_themes().iter().position(|candidate| *candidate == theme).unwrap_or(0)
}

fn tui_color(color: SyntectColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

fn overlay_style(background: Option<SyntectColor>, foreground: Option<SyntectColor>) -> Style {
    let mut style = Style::default();
    if let Some(background) = background {
        style = style.bg(tui_color(background));
    }
    if let Some(foreground) = foreground {
        style = style.fg(tui_color(foreground));
    }
    style
}

pub fn preview_highlight_styles(diff_theme: DiffTheme) -> PreviewHighlightStyles {
    let settings = &assets().themes.get(diff_theme).settings;
    let selection_background = settings
        .selection
        .or(settings.find_highlight)
        .or(settings.inactive_selection)
        .or(settings.line_highlight)
        .or(settings.background);
    let comment_background = settings
        .find_highlight
        .or(settings.inactive_selection)
        .or(settings.line_highlight)
        .or(settings.selection)
        .or(settings.background);
    let comment_foreground = settings
        .find_highlight_foreground
        .or(settings.inactive_selection_foreground);
    let comment_border = settings
        .accent
        .or(comment_foreground)
        .or(settings.caret)
        .or(settings.foreground)
        .or(comment_background)
        .map(tui_color)
        .unwrap_or(Color::Reset);

    PreviewHighlightStyles {
        selection: overlay_style(selection_background, settings.selection_foreground),
        comment: overlay_style(comment_background, comment_foreground),
        comment_border,
    }
}

/// Lines longer than this skip highlighting entirely. syntect's `regex-fancy`
/// backtracking engine is roughly quadratic in line length on pathological
/// input (minified JS/CSS, generated data) — a single multi-hundred-KB line
/// can take tens of seconds and freeze the render thread.
const MAX_HIGHLIGHT_LINE_LEN: usize = 2000;

/// Match VS Code's default Go indentation width. Ratatui does not expand tab
/// characters into terminal cells, so tabs must be converted after any
/// byte-offset-based highlighting has already been applied.
const TAB_WIDTH: usize = 4;

pub fn expand_tabs(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut column = 0usize;
    spans
        .into_iter()
        .map(|span| {
            if !span.content.contains('\t') {
                column += span.width();
                return span;
            }
            let mut expanded = String::new();
            let mut parts = span.content.split('\t').peekable();
            while let Some(part) = parts.next() {
                expanded.push_str(part);
                column += Span::raw(part).width();
                if parts.peek().is_some() {
                    let spaces = TAB_WIDTH - column % TAB_WIDTH;
                    expanded.push_str(&" ".repeat(spaces));
                    column += spaces;
                }
            }
            Span::styled(expanded, span.style)
        })
        .collect()
}

struct Assets {
    syntaxes: SyntaxSet,
    go_zero: SyntaxSet,
    themes: EmbeddedLazyThemeSet,
}

/// Grammar + theme assets, loaded once (the bundled dumps take a few ms).
fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut go_zero = SyntaxSetBuilder::new();
        go_zero.add(
            SyntaxDefinition::load_from_str(
                include_str!("../syntaxes/go-zero-api.sublime-syntax"),
                true,
                None,
            )
            .expect("embedded go-zero API syntax must be valid"),
        );
        Assets {
            syntaxes: two_face::syntax::extra_newlines(),
            go_zero: go_zero.build(),
            themes: two_face::theme::extra(),
        }
    })
}

fn syntax_for_name<'a>(
    assets: &'a Assets,
    name: &str,
    first_line: Option<&str>,
) -> Option<(&'a SyntaxSet, &'a SyntaxReference)> {
    let ext = name.rsplit('.').next().unwrap_or("");
    if let Some(syntax) = assets.go_zero.find_syntax_by_extension(ext) {
        return Some((&assets.go_zero, syntax));
    }
    assets
        .syntaxes
        .find_syntax_by_extension(ext)
        .or_else(|| assets.syntaxes.find_syntax_by_extension(name))
        .or_else(|| first_line.and_then(|line| assets.syntaxes.find_syntax_by_first_line(line)))
        .map(|syntax| (&assets.syntaxes, syntax))
}

/// Highlight `text` for a file called `name`, up to `max` lines. `None` when
/// no grammar matches (caller falls back to plain lines).
pub fn highlight(
    name: &str,
    text: &str,
    max: usize,
    diff_theme: DiffTheme,
) -> Option<Vec<Line<'static>>> {
    let assets = assets();
    let theme = assets.themes.get(diff_theme);
    let (syntaxes, syntax) = syntax_for_name(assets, name, text.lines().next())?;

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for raw in LinesWithEndings::from(text).take(max) {
        if raw.len() > MAX_HIGHLIGHT_LINE_LEN {
            lines.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_string()));
            continue;
        }
        let Ok(regions) = highlighter.highlight_line(raw, syntaxes) else {
            lines.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_string()));
            continue;
        };
        let spans: Vec<Span<'static>> = regions
            .into_iter()
            .filter_map(|(style, chunk)| {
                let chunk = chunk.trim_end_matches(['\n', '\r']);
                if chunk.is_empty() {
                    return None;
                }
                let fg = style.foreground;
                let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                if style.font_style.contains(FontStyle::BOLD) {
                    out = out.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    out = out.add_modifier(Modifier::ITALIC);
                }
                Some(Span::styled(chunk.to_string(), out))
            })
            .collect();
        lines.push(Line::from(spans));
    }
    Some(lines)
}

/// Stateful per-line highlighter (for diff rendering, where old/new file
/// contexts advance independently). Plain spans when no grammar matches.
pub struct LineHighlighter {
    inner: Option<(HighlightLines<'static>, &'static SyntaxSet)>,
}

impl LineHighlighter {
    pub fn new(name: &str, diff_theme: DiffTheme) -> Self {
        let assets = assets();
        let theme = assets.themes.get(diff_theme);
        Self {
            inner: syntax_for_name(assets, name, None)
                .map(|(syntaxes, syntax)| (HighlightLines::new(syntax, theme), syntaxes)),
        }
    }

    /// Highlight one line (no trailing newline in, none out).
    pub fn line(&mut self, text: &str) -> Vec<Span<'static>> {
        let Some((hl, syntaxes)) = self.inner.as_mut() else {
            return vec![Span::raw(text.to_string())];
        };
        if text.len() > MAX_HIGHLIGHT_LINE_LEN {
            return vec![Span::raw(text.to_string())];
        }
        let with_nl = format!("{text}\n");
        match hl.highlight_line(&with_nl, syntaxes) {
            Ok(regions) => regions
                .into_iter()
                .filter_map(|(style, chunk)| {
                    let chunk = chunk.trim_end_matches(['\n', '\r']);
                    if chunk.is_empty() {
                        return None;
                    }
                    let fg = style.foreground;
                    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                    if style.font_style.contains(FontStyle::BOLD) {
                        out = out.add_modifier(Modifier::BOLD);
                    }
                    if style.font_style.contains(FontStyle::ITALIC) {
                        out = out.add_modifier(Modifier::ITALIC);
                    }
                    Some(Span::styled(chunk.to_string(), out))
                })
                .collect(),
            Err(_) => vec![Span::raw(text.to_string())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions_highlight_with_colors() {
        let lines = highlight("main.rs", "fn main() {}\n", 10, DEFAULT_DIFF_THEME)
            .expect("rust grammar");
        assert_eq!(lines.len(), 1);
        // The `fn` keyword must carry a non-default foreground color.
        let colored = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("fn") && s.style.fg.is_some());
        assert!(colored, "expected a colored keyword span");
        assert_eq!(lines[0].to_string(), "fn main() {}");
    }

    #[test]
    fn tabs_expand_to_four_column_stops_across_styled_spans() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let spans = expand_tabs(vec![Span::styled("\tif", red), Span::styled("\tvalue", blue)]);

        assert_eq!(Line::from(spans.clone()).to_string(), "    if  value");
        assert_eq!(spans[0].style, red);
        assert_eq!(spans[1].style, blue);
    }

    #[test]
    fn extended_grammars_cover_typescript_and_toml() {
        assert!(highlight("app.ts", "const x: string = \"hi\";
", 10, DEFAULT_DIFF_THEME).is_some());
        assert!(highlight("Cargo.toml", "[package]
name = \"x\"
", 10, DEFAULT_DIFF_THEME).is_some());
    }

    #[test]
    fn unknown_extensions_fall_back_to_none() {
        assert!(highlight("data.qqzz", "gibberish content\n", 10, DEFAULT_DIFF_THEME).is_none());
    }

    #[test]
    fn go_zero_api_uses_embedded_syntax() {
        let syntax = assets().go_zero.find_syntax_by_extension("api").unwrap();
        assert_eq!(syntax.name, "go-zero API");

        let source = "syntax = \"v1\"\n\
type LoginReq {\n\
    Username string `json:\"username\"`\n\
}\n\
@server (\n\
    group: auth\n\
)\n\
service user-api {\n\
    @handler Login\n\
    post /login (LoginReq) returns (LoginResp)\n\
}\n";
        let lines = highlight("user.api", source, 20, DEFAULT_DIFF_THEME).unwrap();
        assert_eq!(
            lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            source.trim_end()
        );

        let service_line = &lines[7];
        let keyword = service_line
            .spans
            .iter()
            .find(|span| span.content == "service")
            .unwrap();
        let name = service_line
            .spans
            .iter()
            .find(|span| span.content == "user-api")
            .unwrap();
        assert_ne!(keyword.style.fg, name.style.fg);
        assert!(lines[8].spans.iter().any(|span| span.content == "@handler"));
        assert!(lines[2].spans.iter().any(|span| span.content == "string"));

        let mut line_highlighter = LineHighlighter::new("user.api", DEFAULT_DIFF_THEME);
        let route = line_highlighter.line("post /login (LoginReq) returns (LoginResp)");
        assert_eq!(
            Line::from(route.clone()).to_string(),
            "post /login (LoginReq) returns (LoginResp)"
        );
        assert!(route.iter().any(|span| span.content == "post"));
        assert!(route.iter().any(|span| span.content == "/login"));
    }

    #[test]
    fn pathologically_long_lines_skip_highlighting_instead_of_hanging() {
        let long_line = format!("const x = \"{}\";\n", "a".repeat(MAX_HIGHLIGHT_LINE_LEN + 1));
        let lines = highlight("bundle.min.js", &long_line, 10, DEFAULT_DIFF_THEME)
            .expect("js grammar matches");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), long_line.trim_end_matches('\n'));

        let mut hl = LineHighlighter::new("bundle.min.js", DEFAULT_DIFF_THEME);
        let spans = hl.line(long_line.trim_end_matches('\n'));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, long_line.trim_end_matches('\n'));
    }

    #[test]
    fn diff_themes_roundtrip() {
        let themes = diff_themes();
        assert_eq!(themes.len(), 32);
        for theme in themes {
            assert_eq!(diff_theme_from_name(theme.as_name()), Some(*theme));
            assert_eq!(themes[diff_theme_index(*theme)], *theme);
        }
    }

    #[test]
    fn preview_highlights_use_the_selected_theme_colors() {
        let settings = &assets().themes.get(DEFAULT_DIFF_THEME).settings;
        let highlights = preview_highlight_styles(DEFAULT_DIFF_THEME);

        assert_eq!(
            highlights.selection.bg,
            settings
                .selection
                .or(settings.find_highlight)
                .or(settings.inactive_selection)
                .or(settings.line_highlight)
                .or(settings.background)
                .map(tui_color)
        );
        assert_eq!(
            highlights.comment.bg,
            settings
                .find_highlight
                .or(settings.inactive_selection)
                .or(settings.line_highlight)
                .or(settings.selection)
                .or(settings.background)
                .map(tui_color)
        );
    }
}
