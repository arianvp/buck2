/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! How text takes up a terminal: the columns of characters, the rows that lines wrap to (to cut
//! a long value by the rows it takes on the screen), and where the line editor draws its input.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// A tab moves the cursor to the next multiple of this column.
pub const TAB_STOP: usize = 8;

/// The columns `c` takes on a terminal: 2 for a wide character (CJK, most emoji), 0 for a
/// control character or one that combines with the character before it, 1 otherwise.
pub fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// The columns `text` takes on one row (see [`char_width`]; a tab counts as one column).
pub fn str_width(text: &str) -> usize {
    text.chars()
        .map(|c| if c == '\t' { 1 } else { char_width(c) })
        .sum()
}

/// Where the cursor of a terminal is after text is written, relative to where it started:
/// `row` 0 is the row the text starts on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

/// Whether a character starts an escape sequence, and how far into one the writing is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    /// After `ESC`.
    Started,
    /// In the parameters of a control sequence (`ESC [`).
    Csi,
}

/// Where the line editor (rustyline 18) puts the cursor after writing `text` from `start` on a
/// terminal `columns` wide, as its `calculate_position` computes it, so that the client can
/// tell which rows the editor draws on: `\n` starts a row, a tab moves to the next multiple of
/// `tab_stop`, escape sequences take no room, and a grapheme cluster that does not fit on the
/// row goes to the next one. `width` gives the columns of a grapheme cluster (the editor's
/// `GraphemeClusterMode::width`). A position at the end of a full row is at the start of the
/// next one.
pub fn editor_position(
    text: &str,
    start: Position,
    columns: usize,
    tab_stop: usize,
    width: impl Fn(&str) -> usize,
) -> Position {
    let columns = columns.max(1);
    let mut pos = start;
    let mut measure = EditorMeasure::new(tab_stop, width);
    for g in text.graphemes(true) {
        if g == "\n" {
            pos.row += 1;
            pos.col = 0;
            continue;
        }
        let w = measure.columns(g, pos.col);
        pos.col += w;
        if pos.col > columns {
            pos.row += 1;
            pos.col = w;
        }
    }
    if pos.col == columns {
        pos.col = 0;
        pos.row += 1;
    }
    pos
}

/// Blanks in place of `text`, written from the column `start_col` of a terminal `columns` wide:
/// they move the terminal's cursor as the line editor computes that `text` does (see
/// [`editor_position`]) and show nothing (spaces, over rows that the editor has cleared). A
/// newline stays; a grapheme cluster of `w` columns becomes `w` spaces, and when the editor
/// puts it at the start of the next row (it does not fit), the row is filled with spaces first
/// so that the terminal goes to the next row too; escape sequences are left out.
pub fn blanked(
    text: &str,
    start_col: usize,
    columns: usize,
    tab_stop: usize,
    width: impl Fn(&str) -> usize,
) -> String {
    let columns = columns.max(1);
    let mut out = String::new();
    let mut col = start_col;
    let mut measure = EditorMeasure::new(tab_stop, width);
    for g in text.graphemes(true) {
        if g == "\n" {
            out.push('\n');
            col = 0;
            continue;
        }
        let w = measure.columns(g, col);
        if w == 0 {
            continue;
        }
        if col + w > columns {
            // The terminal waits at the end of a full row: the next character goes to the next.
            out.extend(std::iter::repeat_n(' ', columns.saturating_sub(col)));
            col = 0;
        }
        out.extend(std::iter::repeat_n(' ', w));
        col += w;
    }
    out
}

/// The columns that the line editor counts for each grapheme cluster of a text (see
/// [`editor_position`]).
struct EditorMeasure<W> {
    tab_stop: usize,
    width: W,
    escape: Escape,
}

impl<W: Fn(&str) -> usize> EditorMeasure<W> {
    fn new(tab_stop: usize, width: W) -> Self {
        EditorMeasure {
            tab_stop: tab_stop.max(1),
            width,
            escape: Escape::None,
        }
    }

    /// The columns of `g` (not a newline), written at the column `col`.
    fn columns(&mut self, g: &str, col: usize) -> usize {
        if g == "\t" {
            return self.tab_stop - col % self.tab_stop;
        }
        match self.escape {
            Escape::Started => {
                self.escape = if g == "[" { Escape::Csi } else { Escape::None };
                0
            }
            Escape::Csi => {
                if !(g == ";" || g.bytes().next().is_some_and(|b| b.is_ascii_digit())) {
                    self.escape = Escape::None;
                }
                0
            }
            Escape::None if g == "\x1b" => {
                self.escape = Escape::Started;
                0
            }
            Escape::None => (self.width)(g),
        }
    }
}

/// Writes text to a terminal `columns` wide as the terminal does, counting the rows it takes:
/// a character that does not fit on the row goes to the next one, a tab moves to the next
/// multiple of [`TAB_STOP`], and escape sequences (`ESC [ ... final`) take no room.
struct RowCounter {
    columns: usize,
    /// The row of the last character written, from 0.
    row: usize,
    col: usize,
    escape: Escape,
}

impl RowCounter {
    fn new(columns: usize) -> Self {
        RowCounter {
            columns: columns.max(1),
            row: 0,
            col: 0,
            escape: Escape::None,
        }
    }

    /// Writes `c` (not a newline): the row it is on.
    fn write(&mut self, c: char) -> usize {
        let w = match self.escape {
            Escape::Started => {
                self.escape = if c == '[' { Escape::Csi } else { Escape::None };
                0
            }
            // A control sequence ends with a byte in `@`..=`~`.
            Escape::Csi => {
                if ('@'..='~').contains(&c) {
                    self.escape = Escape::None;
                }
                0
            }
            Escape::None if c == '\x1b' => {
                self.escape = Escape::Started;
                0
            }
            Escape::None if c == '\t' => {
                (TAB_STOP - self.col % TAB_STOP).min(self.columns.saturating_sub(self.col).max(1))
            }
            Escape::None => char_width(c),
        };
        self.col += w;
        if self.col > self.columns {
            self.row += 1;
            self.col = w;
        }
        self.row
    }
}

/// The rows that `line` (without a newline) takes on a terminal `columns` wide; an empty line
/// takes one.
pub fn line_rows(line: &str, columns: usize) -> usize {
    let mut counter = RowCounter::new(columns);
    let mut rows = 1;
    for c in line.chars() {
        rows = counter.write(c) + 1;
    }
    rows
}

/// The start of a text that fits in some rows of a terminal (see [`cut_to_rows`]).
#[derive(Debug, PartialEq, Eq)]
pub struct RowCut<'a> {
    /// The start of the text: whole lines, and the start of the next line if it did not fit.
    /// All of the text if it fits.
    pub shown: &'a str,
    /// Characters of the line cut in the middle that are not shown (0 if no line was).
    pub hidden_chars: usize,
    /// Lines after `shown` (and after the line cut in the middle) that are not shown.
    pub hidden_lines: usize,
}

impl RowCut<'_> {
    /// Whether part of the text is not shown.
    pub fn is_cut(&self) -> bool {
        self.hidden_chars > 0 || self.hidden_lines > 0
    }
}

/// The start of `text` that takes at most `max_rows` rows of a terminal `columns` wide, where
/// long lines wrap: whole lines while they fit, then as much of the next line as fits in the
/// rows left. A trailing newline does not start a line.
pub fn cut_to_rows(text: &str, max_rows: usize, columns: usize) -> RowCut<'_> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut used = 0;
    let mut offset = 0;
    let mut lines = body.split('\n');
    while let Some(line) = lines.next() {
        let rows = line_rows(line, columns);
        if used + rows <= max_rows {
            used += rows;
            offset += line.len() + 1;
            continue;
        }
        let left = max_rows.saturating_sub(used);
        if left == 0 {
            return RowCut {
                shown: text.get(..offset).unwrap_or(text),
                hidden_chars: 0,
                hidden_lines: 1 + lines.count(),
            };
        }
        // The line does not fit: as much of it as fits in the rows left.
        let mut counter = RowCounter::new(columns);
        let mut end = 0;
        for (i, c) in line.char_indices() {
            if counter.write(c) >= left {
                break;
            }
            end = i + c.len_utf8();
        }
        return RowCut {
            shown: text.get(..offset + end).unwrap_or(text),
            hidden_chars: line.get(end..).map_or(0, |rest| rest.chars().count()),
            hidden_lines: lines.count(),
        };
    }
    RowCut {
        shown: text,
        hidden_chars: 0,
        hidden_lines: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(text: &str, rows: usize, columns: usize) -> (&str, usize, usize) {
        let c = cut_to_rows(text, rows, columns);
        (c.shown, c.hidden_chars, c.hidden_lines)
    }

    #[test]
    fn test_widths() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('界'), 2);
        assert_eq!(char_width('\u{301}'), 0);
        assert_eq!(char_width('\x07'), 0);
        assert_eq!(str_width("a界b"), 4);
    }

    #[test]
    fn test_line_rows() {
        assert_eq!(line_rows("", 10), 1);
        assert_eq!(line_rows("abc", 10), 1);
        // A full row does not start the next one.
        assert_eq!(line_rows("abcdefghij", 10), 1);
        assert_eq!(line_rows("abcdefghijk", 10), 2);
        assert_eq!(line_rows(&"x".repeat(100_000), 120), 834);
        // A wide character that does not fit goes to the next row.
        assert_eq!(line_rows("abcdefghi界", 10), 2);
        assert_eq!(line_rows(&"界".repeat(5), 10), 1);
        assert_eq!(line_rows(&"界".repeat(6), 10), 2);
        // Escape sequences take no room.
        assert_eq!(line_rows("\x1b[31mabcdefghij\x1b[0m", 10), 1);
        // A tab moves to the next multiple of 8.
        assert_eq!(line_rows("\tab", 10), 1);
        assert_eq!(line_rows("\t\tab", 10), 2);
        // A terminal 0 columns wide is taken as 1 column wide.
        assert_eq!(line_rows("abc", 0), 3);
    }

    #[test]
    fn test_cut_to_rows_lines() {
        let text: String = (0..100).map(|i| format!("{i}\n")).collect();
        let (shown, chars, lines) = cut(&text, 40, 80);
        assert_eq!(shown.lines().count(), 40);
        assert!(shown.ends_with("39\n"));
        assert_eq!((chars, lines), (0, 60));
        // Everything fits.
        assert_eq!(cut("a\nb\n", 40, 80), ("a\nb\n", 0, 0));
        assert_eq!(cut("a\nb", 2, 80), ("a\nb", 0, 0));
        assert!(!cut_to_rows("a\nb", 2, 80).is_cut());
        assert_eq!(cut("", 40, 80), ("", 0, 0));
        assert_eq!(cut("a\nb\nc", 2, 80), ("a\nb\n", 0, 1));
        assert_eq!(cut("a\nb\nc\n", 2, 80), ("a\nb\n", 0, 1));
        assert_eq!(cut("a\nb\nc\nd", 1, 80), ("a\n", 0, 3));
        assert_eq!(cut("a\nb", 0, 80), ("", 0, 2));
        assert_eq!(cut("\n\n\n", 2, 80), ("\n\n", 0, 1));
    }

    #[test]
    fn test_cut_to_rows_long_line() {
        // One long line: the rows that fit, and the number of characters left.
        let text = "x".repeat(100_000);
        let (shown, chars, lines) = cut(&text, 40, 120);
        assert_eq!(shown.len(), 40 * 120);
        assert_eq!((chars, lines), (100_000 - 4800, 0));
        // Short lines, then a long one, then more lines.
        let text = format!("a\nb\n{}\nc\nd", "y".repeat(50));
        let (shown, chars, lines) = cut(&text, 4, 20);
        assert_eq!(shown, format!("a\nb\n{}", "y".repeat(40)));
        assert_eq!((chars, lines), (10, 2));
        // The rows are used up exactly by whole lines: the next line is not started.
        let (shown, chars, lines) = cut("aaaa\nbbbb\ncccc", 2, 4);
        assert_eq!(shown, "aaaa\nbbbb\n");
        assert_eq!((chars, lines), (0, 1));
        // A line cut after its first row.
        let (shown, chars, lines) = cut("aaaa\nbbbbbb\ncccc", 2, 4);
        assert_eq!(shown, "aaaa\nbbbb");
        assert_eq!((chars, lines), (2, 1));
    }

    #[test]
    fn test_cut_to_rows_multibyte() {
        // Wide characters take two columns, and are never split.
        let text = "界".repeat(30);
        let (shown, chars, lines) = cut(&text, 2, 10);
        assert_eq!(shown, "界".repeat(10));
        assert_eq!((chars, lines), (20, 0));
        // An odd width: 4 wide characters per row.
        let (shown, chars, _) = cut(&text, 2, 9);
        assert_eq!(shown, "界".repeat(8));
        assert_eq!(chars, 22);
        // Multi-byte characters of width 1.
        let text = "é".repeat(25);
        let (shown, chars, _) = cut(&text, 2, 10);
        assert_eq!(shown, "é".repeat(20));
        assert_eq!(chars, 5);
    }

    #[test]
    fn test_editor_position() {
        let w = |g: &str| g.chars().map(char_width).sum::<usize>();
        let p = |row, col| Position { row, col };
        assert_eq!(editor_position("abc", p(0, 0), 10, 8, w), p(0, 3));
        assert_eq!(editor_position("abc", p(0, 8), 10, 8, w), p(1, 1));
        // A full row: the cursor is at the start of the next one.
        assert_eq!(editor_position("abcdefghij", p(0, 0), 10, 8, w), p(1, 0));
        assert_eq!(editor_position("ab\ncd", p(0, 5), 10, 8, w), p(1, 2));
        assert_eq!(editor_position("abcdefghi界", p(0, 0), 10, 8, w), p(1, 2));
        assert_eq!(
            editor_position("\x1b[1mab\x1b[0m", p(0, 0), 10, 8, w),
            p(0, 2)
        );
        assert_eq!(editor_position("a\tb", p(0, 0), 20, 8, w), p(0, 9));
        // A grapheme cluster (e + a combining accent) moves as one.
        assert_eq!(editor_position("e\u{301}", p(0, 0), 10, 8, w), p(0, 1));
    }

    #[test]
    fn test_blanked() {
        let w = |g: &str| g.chars().map(char_width).sum::<usize>();
        assert_eq!(blanked("\nabc\nde", 0, 10, 8, w), "\n   \n  ");
        // Escape sequences and characters of no width are left out.
        assert_eq!(blanked("\x1b[1mab\x1b[0m\u{301}", 0, 10, 8, w), "  ");
        // A wide character that does not fit goes to the next row: the row is filled first.
        assert_eq!(
            blanked("abcdefghi界", 0, 10, 8, w),
            format!("{}{}", " ".repeat(10), "  ")
        );
        assert_eq!(blanked("界", 9, 10, 8, w), "   ");
        // A tab as the editor counts it.
        assert_eq!(blanked("a\tb", 0, 20, 8, w), " ".repeat(9));
        // Blanks take the rows that the text takes, and end where it ends.
        let p = |row, col| Position { row, col };
        for text in [
            "\nQQQ ZZZ ZZZ ZZZ ZZZ)",
            "\n界界界界界界\n\tx\n",
            "\nabcdefghij\nk",
            "\na\x1b[31mb界界界界c\u{301}d",
        ] {
            // (A tab is 8 columns wide at most: narrower terminals are not considered.)
            for columns in [8, 9, 10, 13, 80] {
                let blanks = blanked(text, 0, columns, 8, w);
                assert!(blanks.chars().all(|c| c == ' ' || c == '\n'), "{blanks:?}");
                assert_eq!(
                    editor_position(&blanks, p(0, 0), columns, 8, w),
                    editor_position(text, p(0, 0), columns, 8, w),
                    "{text:?} at {columns} columns",
                );
            }
        }
    }
}
