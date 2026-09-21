// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.

use std::{
    marker::PhantomData,
    ops::{Add, ControlFlow, Deref, DerefMut, Range, Sub},
    path::{Path, PathBuf},
    sync::Arc,
};

use foundry_compilers::artifacts::Sources;

pub trait OffsetType {}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Original;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Expanded;
impl OffsetType for Original {}
impl OffsetType for Expanded {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Offset<T: OffsetType> {
    value: usize,
    _ty: PhantomData<T>,
}

pub type OriginalOffset = Offset<Original>;
pub type ExpandedOffset = Offset<Expanded>;

impl<T: OffsetType> Offset<T> {
    #[inline]
    pub const fn new(offset: usize) -> Self {
        Self { value: offset, _ty: PhantomData }
    }

    #[inline]
    pub const fn get(self) -> usize {
        self.value
    }
}

impl<T: OffsetType> Add<usize> for Offset<T> {
    type Output = Self;

    #[inline]
    fn add(self, rhs: usize) -> Self::Output {
        Self::new(self.get() + rhs)
    }
}

impl<T: OffsetType> Sub<isize> for Offset<T> {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: isize) -> Self::Output {
        Self::new((self.get() as isize - rhs) as usize)
    }
}

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
    pub original_offset: OriginalOffset,
    /// 1-based line number in the **original, unmodified** source corresponding to
    /// `original_offset`.
    pub original_line: usize,
    /// Number of bytes added by this edit (`added.len()`), i.e. the length of the inserted
    /// or replacement text. Used to determine whether an expanded offset falls inside the
    /// span of bytes this edit introduced.
    pub added_len: usize,
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
    pub fn insert(self, original_offset: OriginalOffset) -> EditInfo {
        let AdjustmentEntry { path, text, name, original_loc, sources, offset_adjustments } = self;
        let src = sources.get_mut(path).unwrap();
        let content = Arc::make_mut(&mut src.content);
        let (adjusted, info) =
            offset_adjustments.record(path, original_offset, content.as_str(), text, "");
        content.insert_str(adjusted.get(), text);
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
    pub fn replace(self, original_range: Range<OriginalOffset>) -> Option<EditInfo> {
        let AdjustmentEntry { path, text, name, original_loc, sources, offset_adjustments } = self;
        if original_range.end <= original_range.start {
            return None;
        }
        let adjusted_start = offset_adjustments.adjusted_offset(path, original_range.start);
        let adjusted_end = offset_adjustments.adjusted_offset(path, original_range.end);
        let src = sources.get_mut(path).unwrap();
        let content = Arc::make_mut(&mut src.content);
        let removed = content[adjusted_start.get()..adjusted_end.get()].to_owned();
        let (_, info) =
            offset_adjustments.record(path, original_range.start, content.as_str(), text, &removed);
        content.replace_range(adjusted_start.get()..adjusted_end.get(), text);
        if let Some((_, adj)) = offset_adjustments.last_mut() {
            adj.macro_name = name.map(|s| s.to_string());
            adj.original_location = original_loc;
        }
        Some(info)
    }
}

impl OffsetAdjustment {
    /// Returns the current offset in `path` corresponding to `original_offset` from the HIR,
    /// accounting for all length-changing edits recorded by previous macro rules. This is done
    /// by summing all offset deltas affecting the source code prior to the input `original_offset`.
    pub fn adjusted_offset(&self, path: &Path, original_offset: OriginalOffset) -> ExpandedOffset {
        let delta: isize = self
            .iter()
            .filter(|(p, a)| p == path && a.original_offset <= original_offset)
            .map(|(_, a)| a.delta_offset)
            .sum();
        ExpandedOffset::new((original_offset.get() as isize + delta) as usize)
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
        original_offset: OriginalOffset,
        source: &str,
        added: &str,
        removed: &str,
    ) -> (ExpandedOffset, EditInfo) {
        let adjusted_offset = self.adjusted_offset(path, original_offset);
        let current_line =
            source[..adjusted_offset.get()].bytes().filter(|&b| b == b'\n').count() as isize + 1;
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
                added_len: added.len(),
                delta_offset,
                delta_line,
                macro_name: None,
                original_location: None,
            },
        ));
        (adjusted_offset, EditInfo { expanded_line: current_line, delta_lines: delta_line })
    }

    /// Returns the adjustment whose expanded byte range covers `expanded_offset` in `source`,
    /// if any.
    ///
    /// Adjustments are walked in insertion order. The accumulated byte delta is only applied when
    /// an adjustment's expanded position strictly precedes the query, which correctly handles
    /// out-of-order insertions (a later-recorded adjustment with a lower `original_offset`).
    pub fn find_macro_adjustment_by_offset(
        &self,
        source: &Path,
        expanded_offset: ExpandedOffset,
    ) -> Option<&Adjustment> {
        self.fold(source, expanded_offset, None, |pos, adj, acc| {
            if pos <= expanded_offset && expanded_offset < pos + adj.added_len {
                *acc = Some(adj);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
    }

    /// Maps `expanded_offset` (a byte offset in the post-expansion source) back to the
    /// corresponding byte offset in the original, unmodified source.
    ///
    /// Walks adjustments in insertion order, subtracting each adjustment's `delta_offset` when
    /// its expanded position strictly precedes the query. Returns `expanded_offset` unchanged
    /// when no prior adjustments apply.
    ///
    /// The caller is responsible for ensuring `expanded_offset` is not inside a macro-generated
    /// span (use [`find_macro_adjustment_by_offset`](Self::find_macro_adjustment_by_offset)
    /// to check first).
    pub fn get_original_offset(
        &self,
        source: &Path,
        expanded_offset: ExpandedOffset,
    ) -> OriginalOffset {
        self.fold(
            source,
            expanded_offset,
            OriginalOffset::new(expanded_offset.get()),
            |pos, adj, acc| {
                if pos < expanded_offset {
                    *acc = *acc - adj.delta_offset;
                }
                ControlFlow::Continue(())
            },
        )
    }

    /// Shared iteration kernel for
    /// [`find_macro_adjustment_by_offset`](Self::find_macro_adjustment_by_offset)
    /// and [`get_original_offset`](Self::get_original_offset).
    ///
    /// Walks adjustments for `source` in insertion order, computing each adjustment's expanded
    /// position (`original_offset + accumulated_delta`) and passing it to `f`. The accumulated
    /// delta is advanced only when the expanded position strictly precedes `expanded_offset`.
    /// `f` may return [`ControlFlow::Break`] to stop early.
    fn fold<'a, V>(
        &'a self,
        source: &Path,
        expanded_offset: ExpandedOffset,
        mut acc: V,
        f: impl Fn(ExpandedOffset, &'a Adjustment, &mut V) -> ControlFlow<()>,
    ) -> V {
        let mut accumulated_delta = 0isize;
        for (_, adj) in self.iter().filter(|(p, _)| p == source) {
            let expanded_pos = ExpandedOffset::new(
                (adj.original_offset.get() as isize + accumulated_delta) as usize,
            );
            match f(expanded_pos, adj, &mut acc) {
                ControlFlow::Break(_) => return acc,
                ControlFlow::Continue(_) => {}
            }
            if expanded_pos < expanded_offset {
                accumulated_delta += adj.delta_offset;
            }
        }
        acc
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
        let (offset, _) = adj.record(
            Path::new("foo.sol"),
            OriginalOffset::new(16),
            src,
            "\nfunction baz() public {\n }\n",
            "",
        );
        assert_eq!(offset.get(), 16);
        let mut modified = src.to_string();
        modified.insert_str(16, "\nfunction baz() public {\n }\n");
        let (offset, _) = adj.record(
            Path::new("foo.sol"),
            OriginalOffset::new(16),
            &modified,
            "\nfunction bingbong() public {\n }\n",
            "",
        );
        assert_eq!(offset.get(), 16 + 28);
        adj
    }

    #[test]
    fn test_record() {
        let adj = setup();
        assert_eq!(adj.len(), 2);
        let (path, adjustment) = &adj[0];
        assert_eq!(path, Path::new("foo.sol"));
        assert_eq!(adjustment.original_offset.get(), 16);
        assert_eq!(adjustment.original_line, 2);
        assert_eq!(adjustment.delta_offset, 28);
        assert_eq!(adjustment.delta_line, 3);
        let (path2, adj2) = &adj[1];
        assert_eq!(path2, Path::new("foo.sol"));
        assert_eq!(adj2.original_offset.get(), 16);
        assert_eq!(adj2.original_line, 2);
        assert_eq!(adj2.delta_offset, 33);
        assert_eq!(adj2.delta_line, 3);
    }

    #[test]
    fn test_get_original_offset() {
        let adj = setup();
        // Original byte 16 is shifted forward by 28 + 33 = 61 bytes (two insertions).
        // Expanded byte 77 (= 16 + 61) must map back to original byte 16.
        let offset = adj.get_original_offset(Path::new("foo.sol"), ExpandedOffset::new(77));
        assert_eq!(offset.get(), 16);
    }

    #[test]
    fn test_find_macro_adjustment_by_offset() {
        let adj = setup();
        // Byte 20 falls inside Edit 1's inserted range [16, 44).
        assert!(
            adj.find_macro_adjustment_by_offset(Path::new("foo.sol"), ExpandedOffset::new(20))
                .is_some()
        );
        // Byte 77 is past both inserted ranges and belongs to original content.
        assert!(
            adj.find_macro_adjustment_by_offset(Path::new("foo.sol"), ExpandedOffset::new(77))
                .is_none()
        );
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
        let (offset, _) =
            adj.record(Path::new("foo.sol"), OriginalOffset::new(15), src, added, removed);
        assert_eq!(offset.get(), 15);

        assert_eq!(adj.len(), 1);
        let (path, adjustment) = &adj[0];
        assert_eq!(path, Path::new("foo.sol"));
        assert_eq!(adjustment.original_offset.get(), 15);
        assert_eq!(adjustment.original_line, 2);
        assert_eq!(adjustment.delta_offset, -18); // 8 - 26
        assert_eq!(adjustment.delta_line, -1); // 1 - 2 newlines

        // The closing "}" is at original offset 41; after the replacement it should be at 41 - 18 =
        // 23.
        assert_eq!(adj.adjusted_offset(Path::new("foo.sol"), OriginalOffset::new(41)).get(), 23);
    }

    /// Regression test: `get_original_offset` must only subtract the delta of adjustments whose
    /// expanded position is strictly before the query, not all adjustments in the file.
    ///
    /// Insertion order:
    ///   A (original_offset=3, added_len=3): expanded range [3, 6),  delta=+3
    ///   B (original_offset=7, added_len=3): expanded range [10, 13), delta=+3
    ///
    /// Query at expanded byte 8 (between A and B): only A's delta applies, so original = 8 - 3 = 5.
    /// The bug subtracted both deltas, returning 8 - 3 - 3 = 2.
    #[test]
    fn test_get_original_offset_ignores_later_adjustments() {
        let mut adj = OffsetAdjustment::default();
        let src = "0123456789";
        adj.record(Path::new("x.sol"), OriginalOffset::new(3), src, "AAA", "");
        let mut after_a = src.to_string();
        after_a.insert_str(3, "AAA"); // "012AAA3456789"
        adj.record(Path::new("x.sol"), OriginalOffset::new(7), &after_a, "BBB", ""); // expanded at 10

        // Expanded byte 8 is '5', which is original byte 5. Only A's delta applies.
        assert_eq!(adj.get_original_offset(Path::new("x.sol"), ExpandedOffset::new(8)).get(), 5);
    }

    /// Regression test for issue #26: a later-recorded adjustment with a lower `original_offset`
    /// must not cause `find_macro_adjustment_by_offset` to falsely attribute an offset that falls
    /// between the two insertions.
    ///
    /// Insertion order:
    ///   A (recorded first):  original_offset=10, added_len=3 → expanded range [10, 13)
    ///   B (recorded second): original_offset=2,  added_len=3 → expanded range  [2,  5)
    ///                        (A's delta doesn't shift B because A sits after B in expanded coords)
    #[test]
    fn test_out_of_order_adjustments_no_false_attribution() {
        let mut adj = OffsetAdjustment::default();
        let src = "0123456789abcdefghij"; // 20 bytes, no newlines
        adj.record(Path::new("x.sol"), OriginalOffset::new(10), src, "AAA", "");
        let mut after_a = src.to_string();
        after_a.insert_str(10, "AAA");
        adj.record(Path::new("x.sol"), OriginalOffset::new(2), &after_a, "BBB", "");

        // Byte 4 falls inside B's expanded range [2, 5).
        assert!(
            adj.find_macro_adjustment_by_offset(Path::new("x.sol"), ExpandedOffset::new(4))
                .is_some()
        );
        // Byte 5 is the first byte past B and before A — original content, not macro-generated.
        assert!(
            adj.find_macro_adjustment_by_offset(Path::new("x.sol"), ExpandedOffset::new(5))
                .is_none()
        );
    }
}
