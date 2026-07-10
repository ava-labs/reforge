use std::{
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

/// Byte offset and line-number adjustments accumulated by macro rules that change source text.
/// Each entry is `(path, Adjustment)` where [`Adjustment`] records the original offset and line,
/// and the signed byte and line deltas introduced by the edit.
#[derive(Debug, Default, Clone)]
pub struct OffsetAdjustment(Vec<(PathBuf, Adjustment)>);

#[derive(Debug, Clone)]
pub struct Adjustment {
    pub original_offset: usize,
    pub original_line: usize,
    pub delta_offset: isize,
    pub delta_line: isize,
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
        let delta_offset = added.len() as isize - removed.len() as isize;
        let delta_line = added.bytes().filter(|&b| b == b'\n').count() as isize
            - removed.bytes().filter(|&b| b == b'\n').count() as isize;
        self.push((
            path.to_path_buf(),
            Adjustment { original_offset, original_line, delta_offset, delta_line },
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
    fn test_get_original_line() {
        let adj = setup();
        let line = adj.get_original_line(Path::new("foo.sol"), 2 + 6).expect("Test failed");
        assert_eq!(line, 2);
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
