// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.

use std::{
    ops::{Deref, DerefMut, Range},
    path::{Path, PathBuf},
    sync::Arc,
};

use foundry_compilers::artifacts::Sources;

/// An original source location to which a macro-generated span can be attributed.
#[derive(Debug, Clone)]
pub struct MacroOriginalLocation {
    /// Path of the original (pre-expansion) source file.
    pub file: PathBuf,
    /// 1-based line number in the original source.
    pub line: usize,
    /// 1-based column number in the original source.
    pub col: usize,
}

/// Returned by [`AdjustmentEntry::insert`] and [`AdjustmentEntry::replace`].
///
/// Contains the information needed to understand what was generated:
///
/// ```ignore
/// let info = data.entry(path, text).insert(offset);
/// // info.expanded_line is the 1-based line in the expanded source where the edit landed.
/// // info.delta_lines is the net number of lines added (positive) or removed (negative).
/// ```
#[derive(Debug, Clone, Copy)]
pub struct EditInfo {
    /// 1-based line number in the expanded source where this edit takes effect.
    pub expanded_line: isize,
    /// Net lines added (positive) or removed (negative) by this edit.
    pub delta_lines: isize,
}

/// Byte offset and line-number adjustments accumulated by macro rules that change source text.
/// Each entry is `(path, Adjustment)` where [`Adjustment`] records the original offset and line,
/// and the signed byte and line deltas introduced by the edit.
#[derive(Debug, Default, Clone)]
pub struct OffsetAdjustment(Vec<(PathBuf, Adjustment)>);

/// A single recorded edit and optional macro attribution.
#[derive(Debug, Clone)]
pub struct Adjustment {
    /// Byte offset in the **original, unmodified** source where this edit was applied.
    pub original_offset: usize,
    /// 1-based line number in the **original, unmodified** source corresponding to
    /// `original_offset`.
    pub original_line: usize,
    /// Signed byte-length delta introduced by this edit (`added.len() - removed.len()`).
    pub delta_offset: isize,
    /// Net line delta introduced by this edit (newlines added minus newlines removed).
    pub delta_line: isize,
    /// Name of the macro rule that generated this edit, if registered via
    /// [`AdjustmentEntry::with`].
    pub macro_name: Option<String>,
    /// Original source location to attribute compiler errors in this span to, if registered.
    pub original_location: Option<MacroOriginalLocation>,
}

/// Builder returned by [`crate::PreprocessingData::entry`] that performs an insert or replace
/// and optionally annotates the resulting adjustment with macro attribution.
///
/// Call [`with`](AdjustmentEntry::with) before [`insert`](AdjustmentEntry::insert) or
/// [`replace`](AdjustmentEntry::replace) to attach a macro name and optional original location
/// to the adjustment, so that compiler errors in the generated code can be attributed correctly.
pub struct AdjustmentEntry<'a> {
    path: &'a Path,
    text: &'a str,
    name: Option<&'a str>,
    original_loc: Option<MacroOriginalLocation>,
    sources: &'a mut Sources,
    offset_adjustments: &'a mut OffsetAdjustment,
}

impl<'a> AdjustmentEntry<'a> {
    pub fn new(
        path: &'a Path,
        text: &'a str,
        sources: &'a mut Sources,
        offset_adjustments: &'a mut OffsetAdjustment,
    ) -> Self {
        Self { path, text, name: None, original_loc: None, sources, offset_adjustments }
    }

    /// Attaches macro attribution to this edit. `name` identifies the macro rule;
    /// `original_loc` optionally points back to the location in the original source that
    /// triggered the generation, for more precise error reporting.
    pub fn with(self, name: &'a str, original_loc: Option<MacroOriginalLocation>) -> Self {
        Self { name: Some(name), original_loc, ..self }
    }

    /// Inserts `text` into the source file at the position corresponding to `original_offset`
    /// in the original, unmodified source, and records the edit so that subsequent macro rules
    /// remain correct.
    ///
    /// `original_offset` must be a byte offset derived from a Solar HIR span (i.e. relative to
    /// the unmodified source). The method translates it to the current position in the
    /// already-modified text before performing the insertion.
    pub fn insert(self, original_offset: usize) -> EditInfo {
        let AdjustmentEntry { path, text, name, original_loc, sources, offset_adjustments } = self;
        let src = sources.get_mut(path).unwrap();
        let content = Arc::make_mut(&mut src.content);
        let (adjusted, info) =
            offset_adjustments.record(path, original_offset, content.as_str(), text, "");
        content.insert_str(adjusted, text);
        if let Some((_, adj)) = offset_adjustments.last_mut() {
            adj.macro_name = name.map(|s| s.to_string());
            adj.original_location = original_loc;
        }
        info
    }

    /// Replaces the source bytes at `original_range` (in the original, unmodified file) with
    /// `text`, and records the resulting length delta so that subsequent calls using offsets
    /// derived from the original source remain correct.
    ///
    /// Both range endpoints are translated through any previously recorded adjustments before the
    /// replacement is applied. Does nothing and returns `None` if `original_range` is empty or
    /// inverted.
    pub fn replace(self, original_range: Range<usize>) -> Option<EditInfo> {
        let AdjustmentEntry { path, text, name, original_loc, sources, offset_adjustments } = self;
        if original_range.end <= original_range.start {
            return None;
        }
        let adjusted_start = offset_adjustments.adjusted_offset(path, original_range.start);
        let adjusted_end = offset_adjustments.adjusted_offset(path, original_range.end);
        let src = sources.get_mut(path).unwrap();
        let content = Arc::make_mut(&mut src.content);
        let removed = content[adjusted_start..adjusted_end].to_owned();
        let (_, info) =
            offset_adjustments.record(path, original_range.start, content.as_str(), text, &removed);
        content.replace_range(adjusted_start..adjusted_end, text);
        if let Some((_, adj)) = offset_adjustments.last_mut() {
            adj.macro_name = name.map(|s| s.to_string());
            adj.original_location = original_loc;
        }
        Some(info)
    }
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
    fn record(
        &mut self,
        path: &Path,
        original_offset: usize,
        source: &str,
        added: &str,
        removed: &str,
    ) -> (usize, EditInfo) {
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
            Adjustment {
                original_offset,
                original_line,
                delta_offset,
                delta_line,
                macro_name: None,
                original_location: None,
            },
        ));
        (adjusted_offset, EditInfo { expanded_line: current_line, delta_lines: delta_line })
    }

    /// Maps a line number in the macro-expanded source back to the corresponding line number
    /// in the original source.
    ///
    /// Walks the adjustments in order, tracking each insertion's expanded position. For each
    /// adjustment whose expanded position precedes `line`, subtracts its `delta_line` from the
    /// running total.
    ///
    /// Callers are responsible for ensuring `line` is not inside a macro-generated block (use
    /// [`find_macro_adjustment`](Self::find_macro_adjustment) to check first).
    pub fn get_original_line(&self, source: &Path, line: isize) -> usize {
        let mut accumulated_delta = 0isize;
        for (_, adj) in self.iter().filter(|(p, _)| p == source) {
            let expanded_pos = adj.original_line as isize + accumulated_delta;
            if expanded_pos < line {
                accumulated_delta += adj.delta_line;
            }
        }
        (line - accumulated_delta) as usize
    }

    /// Returns the adjustment whose expanded line range covers `line` in `source`, if any.
    ///
    /// Used by error reporting to retrieve any macro attribution registered for the span
    /// that produced a compiler error.
    pub fn find_macro_adjustment(&self, source: &Path, line: isize) -> Option<&Adjustment> {
        let mut accumulated_delta = 0isize;
        for (_, adj) in self.iter().filter(|(p, _)| p == source) {
            let expanded_pos = adj.original_line as isize + accumulated_delta;
            if expanded_pos <= line && line < expanded_pos + adj.delta_line {
                return Some(adj);
            }
            accumulated_delta += adj.delta_line;
        }
        None
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
        let (offset, _) =
            adj.record(Path::new("foo.sol"), 16, src, "\nfunction baz() public {\n }\n", "");
        assert_eq!(offset, 16);
        let mut modified = src.to_string();
        modified.insert_str(16, "\nfunction baz() public {\n }\n");
        let (offset, _) = adj.record(
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
        let [(path, adjustment), (path2, adj2)] = adj.as_slice() else {
            panic!("expected 2 adjustments, got {}", adj.len());
        };
        assert_eq!(path, Path::new("foo.sol"));
        assert_eq!(adjustment.original_offset, 16);
        assert_eq!(adjustment.original_line, 2);
        assert_eq!(adjustment.delta_offset, 28);
        assert_eq!(adjustment.delta_line, 3);
        assert_eq!(path2, Path::new("foo.sol"));
        assert_eq!(adj2.original_offset, 16);
        assert_eq!(adj2.original_line, 2);
        assert_eq!(adj2.delta_offset, 33);
        assert_eq!(adj2.delta_line, 3);
    }

    #[test]
    fn test_get_original_line() {
        let adj = setup();
        let line = adj.get_original_line(Path::new("foo.sol"), 2 + 6);
        assert_eq!(line, 2);
    }

    #[test]
    fn test_is_macro() {
        let adj = setup();
        assert!(adj.find_macro_adjustment(Path::new("foo.sol"), 4).is_some());
        assert!(adj.find_macro_adjustment(Path::new("foo.sol"), 8).is_none());
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
        let (offset, _) = adj.record(Path::new("foo.sol"), 15, src, added, removed);
        assert_eq!(offset, 15);

        let [(path, adjustment)] = adj.as_slice() else {
            panic!("expected 1 adjustment, got {}", adj.len());
        };
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
