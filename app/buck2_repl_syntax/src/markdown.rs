/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Markdown for the terminal. `:doc` and `:qdoc` show documentation that starlark and the query
//! functions write in Markdown (`## ctx.configured\_targets`, fenced code, `*target
//! expression*`, ...); the client renders it as text with [`render`]:
//!
//! - Headings are bold lines (plain lines without colour).
//! - Code blocks lose their fences and are indented; Starlark code is highlighted with colour.
//! - `*emphasis*` is italic and `**strong**` bold (plain text without colour); `` `code` ``
//!   is coloured without its backquotes (kept without colour); links show their text and their
//!   target; backslash escapes (`\_`) and the common entities (`&lt;`) are replaced by their
//!   characters.
//! - The lines of a paragraph are joined (as in Markdown) and wrapped at spaces to the width;
//!   a list item starts a line of its own (even inside a paragraph, and however it is
//!   indented), and its lines wrap under its text. Blank lines are kept, several as one.
//! - The rows of a table (lines that start with `|`) are lines of their own, shown as they are
//!   (their columns stay aligned).
//!
//! Only the constructs that documentation uses are known: the lines of anything else (a block
//! quote, HTML) are paragraph text, their marks shown as they are.

use std::cell::Cell as StdCell;

use crate::highlight;
use crate::terminal::char_width;
use crate::terminal::str_width;

/// How the rendered text is styled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Styling {
    /// Plain text, without escape sequences: inline code keeps its backquotes.
    Plain,
    /// Bold, italic, faint and coloured text (ANSI SGR escape sequences).
    Ansi,
}

/// The style of a character: a set of the flags below.
type Style = u8;
const BOLD: Style = 1;
const ITALIC: Style = 2;
const CODE: Style = 4;
const DIM: Style = 8;

/// A character of the rendered text, with its style.
type Cell = (char, Style);

/// Code blocks are indented by this much.
const CODE_INDENT: &str = "    ";

/// A thematic break (`---`) is a line of this many `─`.
const RULE_WIDTH: usize = 40;

/// The work that rendering the inline Markdown of a paragraph may take: this many steps (about
/// a character looked at each) per byte of it, plus [`INLINE_WORK_BASE`]. Finding where an
/// emphasis, a code span or a link ends scans ahead, possibly to the end of the paragraph, for
/// each delimiter: when many delimiters close nothing, that would take time that grows with the
/// square (or, code spans inside emphasis, the cube) of the paragraph's length. A paragraph that
/// needs more only gets its backslash escapes replaced ([`unescape_only`]).
const INLINE_WORK_PER_BYTE: usize = 32;
const INLINE_WORK_BASE: usize = 4096;

/// Emphasis nested deeper than this is shown as it is.
const MAX_INLINE_DEPTH: usize = 8;

/// Renders `markdown` for a terminal: see the module documentation. Lines are wrapped at
/// `width` columns. The text ends without a newline.
pub fn render(markdown: &str, width: usize, styling: Styling) -> String {
    let mut r = Renderer {
        out: String::with_capacity(markdown.len()),
        width: width.max(1),
        styling,
        blank: false,
    };
    let mut fence: Option<Fence> = None;
    let mut code: Vec<&str> = Vec::new();
    // The paragraph (or list item) whose lines are being read.
    let mut para: Option<Paragraph> = None;
    // The previous line was blank, or a line of an indented code block.
    let mut code_may_start = true;
    for line in markdown.lines() {
        if let Some(open) = &fence {
            if open.is_closed_by(line) {
                r.code_block(&code, open.starlark);
                code.clear();
                fence = None;
            } else {
                code.push(open.dedent(line));
            }
            continue;
        }
        let trimmed = line.trim_end();
        let text = trimmed.trim_start();
        let indent = trimmed.len() - text.len();
        if text.is_empty() {
            r.paragraph(para.take());
            r.blank();
            code_may_start = true;
            continue;
        }
        if let Some(open) = Fence::open(line) {
            r.paragraph(para.take());
            fence = Some(open);
            code_may_start = false;
            continue;
        }
        if is_rule(text) {
            r.paragraph(para.take());
            r.rule();
        } else if text.starts_with('|') {
            // A row of a table: a line of its own, as it is (its columns are aligned).
            r.paragraph(para.take());
            r.code_line(trimmed);
        } else if let Some(item) = list_item(text) {
            // A list item starts a paragraph of its own, even in a paragraph.
            r.paragraph(para.take());
            let marker = match (item.ordered, styling) {
                (Some(number), _) => number,
                (None, Styling::Ansi) => "•",
                (None, Styling::Plain) => "-",
            };
            para = Some(Paragraph {
                indent,
                marker: Some(marker.to_owned()),
                text: item.text.to_owned(),
                style: 0,
            });
        } else if para.is_none() && indent >= 4 && code_may_start {
            // An indented code block, as it is.
            r.code_line(trimmed);
            continue;
        } else if let Some(heading) = heading(text) {
            r.paragraph(para.take());
            r.paragraph(Some(Paragraph {
                indent,
                marker: None,
                text: heading.to_owned(),
                style: BOLD,
            }));
        } else if let Some(p) = &mut para {
            // The next line of a paragraph: the lines of a paragraph are joined (a line break
            // in Markdown is a space).
            p.text.push(' ');
            p.text.push_str(text);
        } else {
            para = Some(Paragraph {
                indent,
                marker: None,
                text: text.to_owned(),
                style: 0,
            });
        }
        code_may_start = false;
    }
    r.paragraph(para);
    if let Some(open) = fence {
        // A block that is not closed ends with the text.
        r.code_block(&code, open.starlark);
    }
    r.out
}

/// A paragraph, a list item or a heading: inline Markdown, wrapped when rendered.
struct Paragraph {
    /// The indentation of its first line.
    indent: usize,
    /// The marker of a list item (`-`, `1.`), before the text.
    marker: Option<String>,
    /// Its lines, joined.
    text: String,
    /// The style of all of its text (a heading is bold).
    style: Style,
}

/// One line of inline Markdown (no blocks: no headings, lists, code blocks) as plain text:
/// emphasis marks, escapes and link syntax are removed, code keeps its backquotes.
pub fn inline_plain(text: &str) -> String {
    let mut cells = Vec::new();
    inline_paragraph(text, 0, Styling::Plain, &mut cells);
    cells.into_iter().map(|(c, _)| c).collect()
}

/// A fenced code block that is open.
struct Fence {
    /// The fence character (`` ` `` or `~`) and how many open the block.
    ch: char,
    len: usize,
    /// The indentation of the opening fence, removed from the lines of the block.
    indent: usize,
    /// The block is Starlark (or Python): it is highlighted.
    starlark: bool,
}

impl Fence {
    fn open(line: &str) -> Option<Fence> {
        let text = line.trim_start();
        let indent = line.len() - text.len();
        if indent > 3 {
            return None;
        }
        let ch = text.chars().next().filter(|c| *c == '`' || *c == '~')?;
        let len = text.chars().take_while(|c| *c == ch).count();
        if len < 3 {
            return None;
        }
        let info = text.get(len..).unwrap_or("").trim();
        if ch == '`' && info.contains('`') {
            return None;
        }
        let language = info.split_whitespace().next().unwrap_or("");
        Some(Fence {
            ch,
            len,
            indent,
            starlark: matches!(
                language,
                "python" | "py" | "starlark" | "star" | "bzl" | "bxl"
            ),
        })
    }

    fn is_closed_by(&self, line: &str) -> bool {
        let text = line.trim();
        let len = text.chars().take_while(|c| *c == self.ch).count();
        len >= self.len && text.chars().all(|c| c == self.ch)
    }

    /// `line` without the indentation of the fence.
    fn dedent<'a>(&self, line: &'a str) -> &'a str {
        let spaces = line
            .bytes()
            .take(self.indent)
            .take_while(|b| *b == b' ')
            .count();
        line.get(spaces..).unwrap_or(line)
    }
}

/// A list item: its marker (`-`, `1.`) and its text.
struct ListItem<'a> {
    /// The marker, as shown (`•`/`-` for an unordered list).
    ordered: Option<&'a str>,
    text: &'a str,
}

/// `text` (a line without its indentation) as a list item, if it is one.
fn list_item(text: &str) -> Option<ListItem<'_>> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    let (marker_end, ordered) = if matches!(first, '-' | '*' | '+') {
        (1, None)
    } else if first.is_ascii_digit() {
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 9 || !matches!(text.as_bytes().get(digits), Some(b'.' | b')')) {
            return None;
        }
        (digits + 1, text.get(..digits + 1))
    } else {
        return None;
    };
    let rest = text.get(marker_end..)?;
    if rest.is_empty() {
        return Some(ListItem { ordered, text: "" });
    }
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(ListItem {
        ordered,
        text: rest.trim_start(),
    })
}

/// The text of a heading (`## text`), if `text` is one.
fn heading(text: &str) -> Option<&str> {
    let level = text.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = text.get(level..)?;
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let rest = rest.trim();
    // A closing sequence of `#`s, after a space, is not part of the text.
    let without_closing = rest.trim_end_matches('#');
    Some(
        if without_closing.is_empty() || without_closing.ends_with([' ', '\t']) {
            without_closing.trim_end()
        } else {
            rest
        },
    )
}

/// Whether `text` is a thematic break: three or more `-`, `*` or `_` (and spaces).
fn is_rule(text: &str) -> bool {
    let Some(ch) = text.chars().next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    text.chars().all(|c| c == ch || c == ' ' || c == '\t')
        && text.chars().filter(|c| *c == ch).count() >= 3
}

/// What is left of the work allowed for rendering the inline Markdown of a paragraph (see
/// [`INLINE_WORK_PER_BYTE`]).
struct Budget {
    left: StdCell<usize>,
    /// The work needed more than was allowed: what was rendered is not complete.
    exceeded: StdCell<bool>,
}

impl Budget {
    fn for_text(text: &str) -> Budget {
        Budget {
            left: StdCell::new(
                text.len()
                    .saturating_mul(INLINE_WORK_PER_BYTE)
                    .saturating_add(INLINE_WORK_BASE),
            ),
            exceeded: StdCell::new(false),
        }
    }

    /// Spends `steps`: false (from then on) if there is not that much left.
    fn spend(&self, steps: usize) -> bool {
        match self.left.get().checked_sub(steps) {
            Some(left) if !self.exceeded.get() => {
                self.left.set(left);
                true
            }
            _ => {
                self.exceeded.set(true);
                false
            }
        }
    }
}

/// Appends the rendering of the inline Markdown `text` (a paragraph, its lines joined) to `out`,
/// every character with the style `base` added; only its backslash escapes are replaced if that
/// takes more work than allowed (see [`INLINE_WORK_PER_BYTE`]).
fn inline_paragraph(text: &str, base: Style, styling: Styling, out: &mut Vec<Cell>) {
    let budget = Budget::for_text(text);
    let mark = out.len();
    inline(text, base, styling, 0, &budget, out);
    if budget.exceeded.get() {
        out.truncate(mark);
        unescape_only(text, base, out);
    }
}

/// Appends the rendering of the inline Markdown `text` to `out`, every character with the style
/// `base` added. Stops early when `budget` runs out (the caller then renders `text` otherwise).
fn inline(
    text: &str,
    base: Style,
    styling: Styling,
    depth: usize,
    budget: &Budget,
    out: &mut Vec<Cell>,
) {
    if depth > MAX_INLINE_DEPTH {
        unescape_only(text, base, out);
        return;
    }
    let push =
        |out: &mut Vec<Cell>, s: &str, style: Style| out.extend(s.chars().map(|c| (c, style)));
    let mut i = 0;
    while let Some(c) = text.get(i..).and_then(|rest| rest.chars().next()) {
        if !budget.spend(1) {
            return;
        }
        let rest = text.get(i..).unwrap_or("");
        match c {
            '\\' => {
                match rest.chars().nth(1) {
                    Some(next) if next.is_ascii_punctuation() => {
                        out.push((next, base));
                        i += 1 + next.len_utf8();
                    }
                    _ => {
                        out.push(('\\', base));
                        i += 1;
                    }
                }
                continue;
            }
            '`' => {
                let run = backquotes(rest);
                if let Some((content, end)) = code_span(text, i, run, budget) {
                    match styling {
                        Styling::Plain => push(out, text.get(i..end).unwrap_or(""), base | CODE),
                        Styling::Ansi => push(out, content, base | CODE),
                    }
                    i = end;
                } else {
                    push(out, rest.get(..run).unwrap_or(""), base);
                    i += run;
                }
                continue;
            }
            '[' => {
                if let Some(end) = link(text, i, base, styling, depth, budget, out) {
                    i = end;
                    continue;
                }
            }
            '<' => {
                // An autolink: `<https://...>` (no spaces in it).
                let end = rest
                    .get(1..)
                    .and_then(|r| r.find(|c: char| c == '>' || c == '<' || c.is_whitespace()))
                    .map(|e| e + 1);
                if !budget.spend(end.unwrap_or(rest.len())) {
                    return;
                }
                if let Some(close) =
                    end.filter(|e| rest.get(*e..).is_some_and(|r| r.starts_with('>')))
                {
                    let target = rest.get(1..close).unwrap_or("");
                    if target.starts_with("http://") || target.starts_with("https://") {
                        push(out, target, base);
                        i += close + 1;
                        continue;
                    }
                }
            }
            '*' | '_' => {
                if let Some((inner, style, end)) = emphasis(text, i, c, budget) {
                    inline(inner, base | style, styling, depth + 1, budget, out);
                    i = end;
                    continue;
                }
                // Not an emphasis: the whole run of delimiters as it is.
                let run = rest.chars().take_while(|d| *d == c).count();
                push(out, rest.get(..run).unwrap_or(""), base);
                i += run;
                continue;
            }
            '&' => {
                if let Some((ch, len)) = entity(rest) {
                    out.push((ch, base));
                    i += len;
                    continue;
                }
            }
            _ => {}
        }
        out.push((c, base));
        i += c.len_utf8();
    }
}

/// Replaces the backslash escapes of `text`, and nothing else.
fn unescape_only(text: &str, base: Style, out: &mut Vec<Cell>) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.peek().copied().filter(char::is_ascii_punctuation) {
                chars.next();
                out.push((next, base));
                continue;
            }
        }
        out.push((c, base));
    }
}

/// The length of the run of backquotes that `text` starts with.
fn backquotes(text: &str) -> usize {
    text.bytes().take_while(|b| *b == b'`').count()
}

/// The code span that starts at `start` of `text` with `run` backquotes: its content and where
/// it ends. `None` if no run of as many backquotes closes it (or `budget` runs out).
fn code_span<'a>(
    text: &'a str,
    start: usize,
    run: usize,
    budget: &Budget,
) -> Option<(&'a str, usize)> {
    let from = start + run;
    let mut j = from;
    while let Some(rest) = text.get(j..) {
        let Some(offset) = rest.find('`') else {
            budget.spend(rest.len());
            return None;
        };
        let at = j + offset;
        let len = backquotes(text.get(at..)?);
        if !budget.spend(offset + len) {
            return None;
        }
        if len == run {
            let content = text.get(from..at)?;
            // One space on each side is not part of the code (it lets code start with `).
            let content = match content.strip_prefix(' ').and_then(|c| c.strip_suffix(' ')) {
                Some(inner) if !inner.trim().is_empty() => inner,
                _ => content,
            };
            return Some((content, at + len));
        }
        j = at + len;
    }
    None
}

/// A link that starts at `start` (`[`) of `text`: `[text](target)` is rendered as its text and
/// its target (`text (target)`), and `[`code`]` (a reference to an item, as in Rust
/// documentation) as the code. Returns where it ends, `None` (and renders nothing) if there is
/// no link at `start`.
fn link(
    text: &str,
    start: usize,
    base: Style,
    styling: Styling,
    depth: usize,
    budget: &Budget,
    out: &mut Vec<Cell>,
) -> Option<usize> {
    let close = matching_bracket(text, start, budget)?;
    let label = text.get(start + 1..close)?;
    let after = text.get(close + 1..)?;
    if after.starts_with('(') {
        let paren = after.find(')');
        if !budget.spend(paren.unwrap_or(after.len())) {
            return None;
        }
        let paren = paren?;
        let target = after.get(1..paren)?.split_whitespace().next().unwrap_or("");
        inline(label, base, styling, depth + 1, budget, out);
        // A link to a heading of the same page (`#name`) leads nowhere on a terminal.
        if !target.is_empty() && !target.starts_with('#') && target != label.trim() {
            out.extend(" (".chars().map(|c| (c, base)));
            out.extend(target.chars().map(|c| (c, base | DIM)));
            out.push((')', base));
        }
        return Some(close + 1 + paren + 1);
    }
    let label = label.trim();
    let run = backquotes(label);
    if run > 0 && code_span(label, 0, run, budget).is_some_and(|(_, end)| end == label.len()) {
        inline(label, base, styling, depth + 1, budget, out);
        return Some(close + 1);
    }
    None
}

/// The `]` that closes the `[` at `start` of `text` (brackets nest; escaped ones and those in
/// code spans do not count).
fn matching_bracket(text: &str, start: usize, budget: &Budget) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = start;
    while let Some(c) = text.get(j..).and_then(|rest| rest.chars().next()) {
        if !budget.spend(1) {
            return None;
        }
        match c {
            '\\' => {
                j += 1 + text.get(j + 1..)?.chars().next().map_or(0, char::len_utf8);
                continue;
            }
            '`' => {
                let run = backquotes(text.get(j..)?);
                j = code_span(text, j, run, budget).map_or(j + run, |(_, end)| end);
                continue;
            }
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j += c.len_utf8();
    }
    None
}

/// An emphasis that starts at `start` of `text` with the delimiter `d` (`*` or `_`): its
/// text, its style and where it ends. One delimiter is italic, two bold, three both. The
/// opening run must be followed by a character that is not a space, the closing one preceded
/// by one; with `_`, neither may be inside a word (`snake_case`).
fn emphasis<'a>(
    text: &'a str,
    start: usize,
    d: char,
    budget: &Budget,
) -> Option<(&'a str, Style, usize)> {
    let rest = text.get(start..)?;
    let run = rest.chars().take_while(|c| *c == d).count();
    let style = match run {
        1 => ITALIC,
        2 => BOLD,
        3 => BOLD | ITALIC,
        _ => return None,
    };
    let from = start + run;
    let next = text.get(from..)?.chars().next()?;
    if next.is_whitespace() {
        return None;
    }
    if d == '_'
        && text
            .get(..start)?
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
    {
        return None;
    }
    let mut j = from;
    while let Some(c) = text.get(j..).and_then(|r| r.chars().next()) {
        if !budget.spend(1) {
            return None;
        }
        match c {
            '\\' => {
                j += 1 + text.get(j + 1..)?.chars().next().map_or(0, char::len_utf8);
                continue;
            }
            '`' => {
                let len = backquotes(text.get(j..)?);
                j = code_span(text, j, len, budget).map_or(j + len, |(_, end)| end);
                continue;
            }
            _ if c == d => {
                let len = text.get(j..)?.chars().take_while(|x| *x == d).count();
                let before = text.get(..j)?.chars().next_back();
                let after = text.get(j + len..)?.chars().next();
                let closes = len == run
                    && j > from
                    && before.is_some_and(|b| !b.is_whitespace())
                    && !(d == '_' && after.is_some_and(char::is_alphanumeric));
                if closes {
                    return Some((text.get(from..j)?, style, j + len));
                }
                j += len;
                continue;
            }
            _ => {}
        }
        j += c.len_utf8();
    }
    None
}

/// The character of the entity that `text` starts with (`&lt;`), and its length.
fn entity(text: &str) -> Option<(char, usize)> {
    const ENTITIES: &[(&str, char)] = &[
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&amp;", '&'),
        ("&quot;", '"'),
        ("&#39;", '\''),
        ("&apos;", '\''),
        ("&nbsp;", ' '),
    ];
    ENTITIES
        .iter()
        .find(|(name, _)| text.starts_with(name))
        .map(|(name, ch)| (*ch, name.len()))
}

/// Writes the rendered lines.
struct Renderer {
    out: String,
    width: usize,
    styling: Styling,
    /// A blank line is due before the next line (blank lines are written only between lines,
    /// and several as one).
    blank: bool,
}

impl Renderer {
    fn blank(&mut self) {
        self.blank = true;
    }

    /// Starts a new line of output.
    fn start_line(&mut self) {
        if !self.out.is_empty() {
            self.out.push('\n');
            if self.blank {
                self.out.push('\n');
            }
        }
        self.blank = false;
    }

    /// A line of text, with its style.
    fn line(&mut self, cells: &[Cell]) {
        self.start_line();
        let mut style: Style = 0;
        for (c, s) in cells {
            if self.styling == Styling::Ansi && *s != style {
                self.out.push_str(&sgr(*s));
                style = *s;
            }
            self.out.push(*c);
        }
        if style != 0 {
            self.out.push_str(highlight::RESET);
        }
        // Spaces at the end of a line show nothing.
        let kept = self.out.trim_end_matches([' ', '\t']).len();
        self.out.truncate(kept);
    }

    /// A line that is shown as it is.
    fn code_line(&mut self, text: &str) {
        self.start_line();
        self.out.push_str(text);
    }

    /// A fenced code block, set apart from the text around it by blank lines.
    fn code_block(&mut self, lines: &[&str], starlark: bool) {
        if lines.is_empty() {
            return;
        }
        self.blank = true;
        let text = lines.join("\n");
        let painted = if starlark && self.styling == Styling::Ansi {
            let spans = highlight::spans(&text, None);
            highlight::paint(&text, &spans, highlight::ansi_style)
        } else {
            text
        };
        for line in painted.split('\n') {
            self.start_line();
            if !line.is_empty() {
                self.out.push_str(CODE_INDENT);
                self.out.push_str(line.trim_end_matches([' ', '\t']));
            }
        }
        self.blank = true;
    }

    fn rule(&mut self) {
        let rule: Vec<Cell> = std::iter::repeat_n(('─', DIM), RULE_WIDTH.min(self.width)).collect();
        self.line(&rule);
    }

    fn paragraph(&mut self, para: Option<Paragraph>) {
        let Some(para) = para else {
            return;
        };
        let mut prefix: Vec<Cell> = Vec::new();
        if let Some(marker) = &para.marker {
            prefix.extend(marker.chars().map(|c| (c, 0)));
            prefix.push((' ', 0));
        }
        let mut cells = Vec::new();
        inline_paragraph(&para.text, para.style, self.styling, &mut cells);
        self.wrapped(para.indent, &prefix, &cells);
    }

    /// `prefix` then `cells`, indented by `indent` spaces, wrapped at spaces to the width: the
    /// lines after the first are indented to where the text after `prefix` starts.
    fn wrapped(&mut self, indent: usize, prefix: &[Cell], cells: &[Cell]) {
        let hanging = indent + prefix.len();
        let mut line: Vec<Cell> = std::iter::repeat_n((' ', 0), indent).collect();
        line.extend_from_slice(prefix);
        let mut width = hanging;
        if width + cells_width(cells) <= self.width {
            line.extend_from_slice(cells);
            self.line(&line);
            return;
        }
        let mut words = cells.split(|(c, _)| *c == ' ').filter(|w| !w.is_empty());
        let mut first = true;
        for word in words.by_ref() {
            let w = cells_width(word);
            if !first && width + 1 + w > self.width {
                self.line(&line);
                line = std::iter::repeat_n((' ', 0), hanging).collect();
                width = hanging;
                first = true;
            }
            if !first {
                // The space has the style that the words on both sides have.
                let before = line.last().map_or(0, |(_, s)| *s);
                let after = word.first().map_or(0, |(_, s)| *s);
                line.push((' ', before & after));
                width += 1;
            }
            line.extend_from_slice(word);
            width += w;
            first = false;
        }
        self.line(&line);
    }
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|(c, _)| char_width(*c)).sum()
}

/// The SGR sequence that sets `style` (after resetting any other).
fn sgr(style: Style) -> String {
    if style == 0 {
        return highlight::RESET.to_owned();
    }
    let mut codes = vec!["0"];
    if style & BOLD != 0 {
        codes.push("1");
    }
    if style & DIM != 0 {
        codes.push("2");
    }
    if style & ITALIC != 0 {
        codes.push("3");
    }
    if style & CODE != 0 {
        codes.push("36");
    }
    format!("\x1b[{}m", codes.join(";"))
}

/// The width of `text` once rendered (for tests and callers that align rendered text).
pub fn rendered_width(line: &str) -> usize {
    let mut width = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // `ESC [ ... m`
            for d in chars.by_ref() {
                if ('@'..='~').contains(&d) && d != '[' {
                    break;
                }
            }
            continue;
        }
        width += str_width(c.encode_utf8(&mut [0; 4]));
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(md: &str) -> String {
        render(md, 88, Styling::Plain)
    }

    fn ansi(md: &str) -> String {
        render(md, 88, Styling::Ansi)
    }

    #[test]
    fn test_headings() {
        assert_eq!(
            plain("## ctx.configured\\_targets"),
            "ctx.configured_targets"
        );
        assert_eq!(
            ansi("## ctx.configured\\_targets"),
            "\x1b[0;1mctx.configured_targets\x1b[0m"
        );
        assert_eq!(plain("# `bxl.Context` type"), "`bxl.Context` type");
        assert_eq!(
            ansi("# `bxl.Context` type"),
            "\x1b[0;1;36mbxl.Context\x1b[0;1m type\x1b[0m"
        );
        assert_eq!(plain("#### Parameters ##"), "Parameters");
        assert_eq!(plain("#hashtag"), "#hashtag");
        assert_eq!(plain("####### seven"), "####### seven");
    }

    #[test]
    fn test_code_blocks() {
        let md = "## f\n\n```python\ndef f(\n    a: int,\n) -> str\n```\n\nText.";
        assert_eq!(
            plain(md),
            "f\n\n    def f(\n        a: int,\n    ) -> str\n\nText."
        );
        // Highlighted with colour.
        let out = ansi("```python\ndef f(x): return \"s\"\n```");
        assert!(out.starts_with("    \x1b[35mdef\x1b[0m f(x)"), "{out:?}");
        assert!(out.contains("\x1b[32m\"s\"\x1b[0m"), "{out:?}");
        // Other code is not.
        assert_eq!(
            ansi("```text\n$ buck2 uquery *x*\n```"),
            "    $ buck2 uquery *x*"
        );
        // Nothing inside a code block is Markdown.
        assert_eq!(
            plain("```\n## not a heading\n`x` \\_\n```"),
            "    ## not a heading\n    `x` \\_"
        );
        // A block that is not closed ends with the text.
        assert_eq!(plain("```\nx = 1"), "    x = 1");
        // An indented code block after a blank line.
        assert_eq!(
            plain("Example:\n\n    x = a_b * c\n"),
            "Example:\n\n    x = a_b * c"
        );
    }

    #[test]
    fn test_inline() {
        assert_eq!(plain("a `b` c"), "a `b` c");
        assert_eq!(ansi("a `b` c"), "a \x1b[0;36mb\x1b[0m c");
        assert_eq!(plain("`` a`b ``"), "`` a`b ``");
        assert_eq!(ansi("`` a`b ``"), "\x1b[0;36ma`b\x1b[0m");
        assert_eq!(plain("unclosed ` tick"), "unclosed ` tick");
        assert_eq!(
            plain(
                "#### rdeps(universe: *target expression*, depth: *integer*, captured_expr: ?*query expression*)"
            ),
            "rdeps(universe: target expression, depth: integer, captured_expr: ?query expression)"
        );
        assert_eq!(ansi("*a* **b**"), "\x1b[0;3ma\x1b[0m \x1b[0;1mb\x1b[0m");
        assert_eq!(plain("***both***"), "both");
        assert_eq!(plain("_x_ and __y__"), "x and y");
        // Not emphasis: snake_case, spaces around, `*args`.
        assert_eq!(
            plain("target_platform and my_var_name"),
            "target_platform and my_var_name"
        );
        assert_eq!(plain("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(plain("f(*args, **kwargs)"), "f(*args, **kwargs)");
        assert_eq!(plain("a ** b"), "a ** b");
        // Not closed by a delimiter in code.
        assert_eq!(plain("*a `b*` c*"), "a `b*` c");
        // Escapes.
        assert_eq!(plain("a\\_b \\* \\` \\\\ \\q"), "a_b * ` \\ \\q");
        assert_eq!(plain("`a\\_b`"), "`a\\_b`");
        assert_eq!(
            plain("&lt;cell&gt;//path &amp; &unknown;"),
            "<cell>//path & &unknown;"
        );
    }

    #[test]
    fn test_links() {
        assert_eq!(
            plain("[len]( https://github.com/bazelbuild/starlark/blob/master/spec.md#len ): get"),
            "len (https://github.com/bazelbuild/starlark/blob/master/spec.md#len): get"
        );
        assert_eq!(
            ansi("[len](https://x.y)"),
            "len (\x1b[0;2mhttps://x.y\x1b[0m)"
        );
        assert_eq!(plain("[https://x.y](https://x.y)"), "https://x.y");
        assert_eq!(plain("a [`TargetListExpr`], b"), "a `TargetListExpr`, b");
        assert_eq!(ansi("[`T`]"), "\x1b[0;36mT\x1b[0m");
        assert_eq!(plain("[not a link] [x]"), "[not a link] [x]");
        assert_eq!(plain("<https://x.y/z>"), "https://x.y/z");
        assert_eq!(plain("a <b> c"), "a <b> c");
        assert_eq!(plain("[unclosed"), "[unclosed");
        // A link to a heading of the page shows its text only.
        assert_eq!(
            plain("opposite of [`attrfilter`](#attrfilter)."),
            "opposite of `attrfilter`."
        );
        assert_eq!(inline_plain("- *a* [`b`](#b) \\_ `c`"), "- a `b` _ `c`");
    }

    #[test]
    fn test_lists_and_rules() {
        let md = "* `name`: the name\n\n  more\n- b\n    - nested\n1. one\n10) ten";
        assert_eq!(
            plain(md),
            "- `name`: the name\n\n  more\n- b\n    - nested\n1. one\n10) ten"
        );
        assert_eq!(ansi("- a"), "• a");
        assert_eq!(plain("-not a list"), "-not a list");
        assert_eq!(
            plain("a\n\n---\n\nb"),
            format!("a\n\n{}\n\nb", "─".repeat(40))
        );
        assert_eq!(plain("* * *"), "─".repeat(40));
    }

    #[test]
    fn test_paragraphs() {
        // The lines of a paragraph are joined; blocks and list items start lines of their own.
        assert_eq!(plain("a\nb\n\nc"), "a b\n\nc");
        assert_eq!(plain("a *b\nc* d"), "a b c d");
        assert_eq!(plain("- a\n  b\n- c"), "- a b\n- c");
        assert_eq!(
            plain("which is either:\n    - a single string\n    - a list"),
            "which is either:\n    - a single string\n    - a list"
        );
        assert_eq!(
            plain("## h\ntext\n---\nmore"),
            format!("h\ntext\n{}\nmore", "─".repeat(40))
        );
        assert_eq!(
            plain("text\n```\ncode\n```\nafter"),
            "text\n\n    code\n\nafter"
        );
        // An indented line in a paragraph is part of it.
        assert_eq!(plain("a\n    b"), "a b");
    }

    #[test]
    fn test_blank_lines() {
        assert_eq!(plain("\n\na\n\n\n\nb\n\n"), "a\n\nb");
        assert_eq!(plain(""), "");
        assert_eq!(plain("a  \nb"), "a b");
    }

    #[test]
    fn test_wrapping() {
        let long = "word ".repeat(30);
        let out = render(&long, 40, Styling::Plain);
        for line in out.lines() {
            assert!(line.len() <= 40, "{line:?}");
        }
        assert_eq!(out.split_whitespace().count(), 30);
        // A hanging indent under a list item's text, and under an indented line.
        let out = render(&format!("- {long}"), 40, Styling::Plain);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("- word"));
        assert!(
            lines[1..].iter().all(|l| l.starts_with("  word")),
            "{lines:?}"
        );
        let out = render(&format!("   - {long}"), 40, Styling::Plain);
        assert!(
            out.lines().skip(1).all(|l| l.starts_with("     word")),
            "{out}"
        );
        // A word longer than the width is not split.
        let out = render(&format!("a {} b", "x".repeat(50)), 40, Styling::Plain);
        assert_eq!(out, format!("a\n{}\nb", "x".repeat(50)));
        // With colour, the width is what shows, and styles carry over to the next line.
        let out = render(&format!("*{}*", long.trim()), 40, Styling::Ansi);
        for line in out.lines() {
            assert!(rendered_width(line) <= 40, "{line:?}");
            assert!(line.starts_with("\x1b[0;3m"), "{line:?}");
            assert!(line.ends_with("\x1b[0m"), "{line:?}");
        }
        // Code is never wrapped.
        let code = format!("```\n{}\n```", "y".repeat(100));
        assert_eq!(
            render(&code, 40, Styling::Plain),
            format!("    {}", "y".repeat(100))
        );
    }

    #[test]
    fn test_no_escape_sequences_when_plain() {
        let md = "# `T` type\n\n## T.f\n\n```python\ndef T.f(a: int) -> str\n```\n\n*x* **y** [z](u)\n\n---";
        assert!(!plain(md).contains('\x1b'));
    }

    #[test]
    fn test_long_lines_and_nesting() {
        // Many delimiters that close nothing: only the escapes are replaced.
        let long = format!("{}a\\_b", "*x ".repeat(5000));
        let out = plain(&long);
        assert!(out.contains("a_b"));
        assert!(out.starts_with("*x *x"));
        // Deep nesting is shown as it is.
        let nested = format!("{}x{}", "*_".repeat(40), "_*".repeat(40));
        let out = plain(&nested);
        assert!(out.contains('x'));
    }

    /// A paragraph whose delimiters close nothing, as many as fit, with code spans that do not
    /// close either: each emphasis looks for its end over the rest of the paragraph, and at each
    /// run of backquotes for the end of a code span.
    fn costly_paragraph(len: usize) -> String {
        let mut para = String::new();
        let mut n = 1;
        while para.len() < len {
            para.push_str(&"*a ".repeat(30));
            para.push_str(&"`".repeat(n));
            para.push(' ');
            para.push_str("[b ");
            n += 1;
        }
        para
    }

    #[test]
    fn test_inline_work_is_bounded() {
        // Such a paragraph takes work that grows with the cube of its length; it only gets its
        // escapes replaced once that is more than the paragraph's allowance.
        let para = format!("{}\\_", costly_paragraph(8000));
        let out = render(&para, 1_000_000, Styling::Plain);
        assert_eq!(out, para.replace("\\_", "_").trim_end());

        // Rendering stays fast: 100 such paragraphs (800 KB) take well under a second in an
        // optimized build (over 30 seconds when the work was not bounded).
        let doc = vec![costly_paragraph(8000); 100].join("\n\n");
        let started = std::time::Instant::now();
        let out = render(&doc, 88, Styling::Ansi);
        let took = std::time::Instant::now().saturating_duration_since(started);
        assert!(!out.is_empty());
        assert!(took < std::time::Duration::from_secs(15), "{took:?}");

        // Ordinary text is rendered with its delimiters, however long the paragraph.
        let long = "*a* `b` [c](d) ".repeat(2000);
        let out = render(&long, 1_000_000, Styling::Plain);
        assert_eq!(out, "a `b` c (d) ".repeat(2000).trim_end());
    }

    #[test]
    fn test_tables() {
        let md = "A table:\n| col a | col b |\n|-------|-------|\n| `1`   | 2\\_    |\nafter it.";
        assert_eq!(
            plain(md),
            "A table:\n| col a | col b |\n|-------|-------|\n| `1`   | 2\\_    |\nafter it."
        );
        // Indented under a list item too.
        assert_eq!(
            plain("- item\n  | a | b |\n  | 1 | 2 |"),
            "- item\n  | a | b |\n  | 1 | 2 |"
        );
    }
}
