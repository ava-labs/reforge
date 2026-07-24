use std::{
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

/// Byte offset, line-number and column-number adjustments accumulated by macro rules that change
/// source text. Each entry is `(path, Adjustment)` where [`Adjustment`] records the original
/// offset, line and column, and the signed byte, line and column deltas introduced by the edit.
#[derive(Debug, Default, Clone)]
pub struct OffsetAdjustment(Vec<(PathBuf, Adjustment)>);

#[derive(Debug, Clone)]
pub struct Adjustment {
    pub original_offset: usize,
    pub original_line: usize,
    /// 1-based column of the edit position in the original source.
    pub original_col: usize,
    pub delta_offset: isize,
    pub delta_line: isize,
    /// Signed change in column applied to content that follows the edit on the same line.
    ///
    /// Only single-line edits (no newline in either the added or removed text) shift the
    /// columns of the content that follows them; multi-line edits push subsequent content
    /// onto fresh lines whose columns are unaffected, so their `delta_col` is `0`.
    pub delta_col: isize,
}

impl OffsetAdjustment {
    /// Returns the current offset in `path` corresponding to `edit_offset` from the HIR,
    /// accounting for all length-changing edits recorded by previous macro rules. This is done
    /// by summing all offset deltas affecting the source code prior to the input `edit_offset`.
    pub fn adjusted_offset(&self, path: &Path, edit_offset: usize) -> usize {
        let delta: isize = self
            .iter()
            .filter(|(p, a)| p == path && a.original_offset <= edit_offset)
            .map(|(_, a)| a.delta_offset)
            .sum();
        (edit_offset as isize + delta) as usize
    }

    /// Records an edit in `path` at `original_offset` in the original source and returns the
    /// adjusted byte offset of the edit in the current (post-prior-edits) source.
    ///
    /// `source` is the source text *before* this edit is applied. `added` is the text being
    /// inserted and `removed` is the text being replaced (pass `""` for pure insertions).
    ///
    /// `original_line` is derived by counting newlines in `source` up to the adjusted offset and
    /// subtracting accumulated line deltas from all previously recorded edits at or before
    /// `original_offset`, mapping the current position back to original-source coordinates.
    ///
    /// `original_col` is derived analogously: the current column (bytes since the last newline)
    /// is mapped back to original coordinates by subtracting the column deltas of prior edits
    /// that lie earlier on the same original line.
    pub fn record(
        &mut self,
        path: &Path,
        original_offset: usize,
        source: &str,
        added: &str,
        removed: &str,
    ) -> usize {
        let adjusted_offset = self.adjusted_offset(path, original_offset);
        let current_line =
            source[..adjusted_offset].bytes().filter(|&b| b == b'\n').count() as isize + 1;
        let accumulated_line_delta: isize = self
            .iter()
            .filter(|(p, a)| p.as_path() == path && a.original_offset <= original_offset)
            .map(|(_, a)| a.delta_line)
            .sum();
        let original_line = (current_line - accumulated_line_delta) as usize;

        // Column of the edit in the current (post-prior-edits) source, then mapped back to
        // original coordinates by undoing the column shifts of earlier same-line edits.
        let line_start = source[..adjusted_offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let current_col = (adjusted_offset - line_start) as isize + 1;
        let accumulated_col_delta: isize = self
            .iter()
            .filter(|(p, a)| {
                p.as_path() == path
                    && a.original_line == original_line
                    && a.original_offset < original_offset
            })
            .map(|(_, a)| a.delta_col)
            .sum();
        let original_col = (current_col - accumulated_col_delta).max(1) as usize;

        let delta_offset = added.len() as isize - removed.len() as isize;
        let added_newlines = added.bytes().filter(|&b| b == b'\n').count() as isize;
        let removed_newlines = removed.bytes().filter(|&b| b == b'\n').count() as isize;
        let delta_line = added_newlines - removed_newlines;
        // Only single-line edits shift the columns of the content that follows them. Multi-line
        // edits move subsequent content onto fresh lines whose columns start over.
        let delta_col = if added_newlines == 0 && removed_newlines == 0 {
            added.len() as isize - removed.len() as isize
        } else {
            0
        };
        self.push((
            path.to_path_buf(),
            Adjustment {
                original_offset,
                original_line,
                original_col,
                delta_offset,
                delta_line,
                delta_col,
            },
        ));
        adjusted_offset
    }

    /// Maps a line number in the macro-expanded source back to the corresponding line number
    /// in the original source, or `None` if `line` falls within a macro-generated block.
    ///
    /// Walks the adjustments in order, tracking each insertion's position in expanded coordinates.
    /// For each adjustment whose expanded position precedes `line`, subtracts its `delta_line` from
    /// the result.
    pub fn get_original_line(&self, source: &Path, line: isize) -> Option<usize> {
        if self.is_macro(source, line) {
            return None;
        }
        let mut accumulated_delta = 0isize;
        for (_, adj) in self.iter().filter(|(p, _)| p == source) {
            let expanded_pos = adj.original_line as isize + accumulated_delta;
            if expanded_pos < line {
                accumulated_delta += adj.delta_line;
            }
        }
        Some((line - accumulated_delta) as usize)
    }

    /// Maps a column number in the macro-expanded source back to the corresponding column in the
    /// original source.
    ///
    /// `original_line` is the line the column lives on *in original coordinates* (i.e. the result
    /// of [`OffsetAdjustment::get_original_line`]). The column is remapped by undoing the shifts
    /// of every same-line edit that precedes it. Only single-line edits carry a non-zero
    /// `delta_col`, so multi-line insertions (whole-line macro blocks) leave columns untouched.
    pub fn get_original_col(&self, source: &Path, original_line: usize, col: isize) -> usize {
        let col_delta: isize = self
            .iter()
            .filter(|(p, a)| {
                p == source && a.original_line == original_line && (a.original_col as isize) < col
            })
            .map(|(_, a)| a.delta_col)
            .sum();
        (col - col_delta).max(1) as usize
    }

    /// Check if a line belongs to macro-generated code.
    ///
    /// Returns `true` when `line` falls within the expanded block inserted by any adjustment, i.e.
    /// in `[expanded_pos, expanded_pos + delta_line)` for some recorded insertion.
    pub fn is_macro(&self, source: &Path, line: isize) -> bool {
        let mut accumulated_delta = 0isize;
        for (_, adj) in self.iter().filter(|(p, _)| p == source) {
            let expanded_pos = adj.original_line as isize + accumulated_delta;
            if expanded_pos <= line && line < expanded_pos + adj.delta_line {
                return true;
            }
            accumulated_delta += adj.delta_line;
        }
        false
    }
}

impl Deref for OffsetAdjustment {
    type Target = Vec<(PathBuf, Adjustment)>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for OffsetAdjustment {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> OffsetAdjustment {
        let mut adj = OffsetAdjustment::default();
        let src = "contract Foo { \nfunction bar() public {\n }\n }";
        let offset =
            adj.record(Path::new("foo.sol"), 16, src, "\nfunction baz() public {\n }\n", "");
        assert_eq!(offset, 16);
        let mut modified = src.to_string();
        modified.insert_str(16, "\nfunction baz() public {\n }\n");
        let offset = adj.record(
            Path::new("foo.sol"),
            16,
            &modified,
            "\nfunction bingbong() public {\n }\n",
            "",
        );
        assert_eq!(offset, 16 + 28);
        adj
    }

    #[test]
    fn test_record() {
        let adj = setup();
        assert_eq!(adj.len(), 2);
        let (path, adjustment) = &adj[0];
        assert_eq!(path, Path::new("foo.sol"));
        assert_eq!(adjustment.original_offset, 16);
        assert_eq!(adjustment.original_line, 2);
        assert_eq!(adjustment.delta_offset, 28);
        assert_eq!(adjustment.delta_line, 3);
        let (path2, adj2) = &adj[1];
        assert_eq!(path2, Path::new("foo.sol"));
        assert_eq!(adj2.original_offset, 16);
        assert_eq!(adj2.original_line, 2);
        assert_eq!(adj2.delta_offset, 33);
        assert_eq!(adj2.delta_line, 3);
    }

    #[test]
    fn test_record_columns_whole_line_insert() {
        // Whole-line macro insertions (text bracketed by newlines) must not shift columns.
        let adj = setup();
        let (_, adjustment) = &adj[0];
        assert_eq!(adjustment.original_col, 1);
        assert_eq!(adjustment.delta_col, 0);
        let (_, adj2) = &adj[1];
        assert_eq!(adj2.original_col, 1);
        assert_eq!(adj2.delta_col, 0);
    }

    #[test]
    fn test_get_original_line() {
        let adj = setup();
        let line = adj.get_original_line(Path::new("foo.sol"), 2 + 6).expect("Test failed");
        assert_eq!(line, 2);
    }

    #[test]
    fn test_inline_replace_shifts_columns() {
        // Replace "library" (7 bytes) with "contract" (8 bytes) at the start of line 1. This is a
        // single-line edit, so content after it on the same line shifts right by one column.
        let mut adj = OffsetAdjustment::default();
        let src = "library Foo {\n}";
        let offset = adj.record(Path::new("foo.sol"), 0, src, "contract", "library");
        assert_eq!(offset, 0);

        let (_, adjustment) = &adj[0];
        assert_eq!(adjustment.original_line, 1);
        assert_eq!(adjustment.original_col, 1);
        assert_eq!(adjustment.delta_offset, 1);
        assert_eq!(adjustment.delta_line, 0);
        assert_eq!(adjustment.delta_col, 1);

        // "Foo" is at column 10 in the expanded "contract Foo" and column 9 in "library Foo".
        assert_eq!(adj.get_original_col(Path::new("foo.sol"), 1, 10), 9);
        // A column before the edit is unchanged.
        assert_eq!(adj.get_original_col(Path::new("foo.sol"), 1, 1), 1);
        // A column on a different line is unaffected by the edit.
        assert_eq!(adj.get_original_col(Path::new("foo.sol"), 2, 5), 5);
    }

    #[test]
    fn test_get_original_col_no_adjustments() {
        let adj = OffsetAdjustment::default();
        assert_eq!(adj.get_original_col(Path::new("foo.sol"), 3, 7), 7);
    }

    #[test]
    fn test_is_macro() {
        let adj = setup();
        assert!(adj.is_macro(Path::new("foo.sol"), 4));
        assert!(!adj.is_macro(Path::new("foo.sol"), 8));
    }

    #[test]
    fn test_record_replace() {
        let mut adj = OffsetAdjustment::default();
        // line 1: "contract Foo {", line 2: "function bar() public {", line 3: "}", line 4: "}"
        let src = "contract Foo {\nfunction bar() public {\n}\n}";
        // Replace "function bar() public {\n}\n" (26 bytes, 2 newlines) with "uint x;\n" (8 bytes,
        // 1 newline)
        let removed = "function bar() public {\n}\n";
        let added = "uint x;\n";
        let offset = adj.record(Path::new("foo.sol"), 15, src, added, removed);
        assert_eq!(offset, 15);

        assert_eq!(adj.len(), 1);
        let (path, adjustment) = &adj[0];
        assert_eq!(path, Path::new("foo.sol"));
        assert_eq!(adjustment.original_offset, 15);
        assert_eq!(adjustment.original_line, 2);
        assert_eq!(adjustment.delta_offset, -18); // 8 - 26
        assert_eq!(adjustment.delta_line, -1); // 1 - 2 newlines

        // The closing "}" is at original offset 41; after the replacement it should be at 41 - 18 =
        // 23.
        assert_eq!(adj.adjusted_offset(Path::new("foo.sol"), 41), 23);
    }
}
