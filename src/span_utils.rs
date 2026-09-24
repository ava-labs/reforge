// Copyright (C) 2026, Ava Labs, Inc.
// See the file LICENSE for licensing terms.

use std::{
    marker::PhantomData,
    ops::{ControlFlow, Deref, DerefMut, Range},
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

impl<T: OffsetType> Offset<T> {
    pub fn checked_add(self, rhs: usize) -> Option<Self> {
        self.get().checked_add(rhs).map(Self::new)
    }

    pub fn checked_sub_isize(self, rhs: isize) -> Option<Self> {
        let lhs = isize::try_from(self.get()).ok()?;
        usize::try_from(lhs.checked_sub(rhs)?).ok().map(Self::new)
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
    ///
    /// Inserts `text` into the source file at the position corresponding to `original_offset`
    /// in the original, unmodified source, and records the edit so that subsequent macro rules
    /// remain correct.
    ///
    /// Returns an error if any byte-offset or line-count arithmetic overflows during adjustment
    /// computation, which would indicate a corrupt or astronomically large source file.
    pub fn insert(self, original_offset: OriginalOffset) -> eyre::Result<EditInfo> {
        let AdjustmentEntry { path, text, name, original_loc, sources, offset_adjustments } = self;
        let src = sources.get_mut(path).unwrap();
        let content = Arc::make_mut(&mut src.content);
        let (adjusted, info) =
            offset_adjustments.record(path, original_offset, content.as_str(), text, "")?;
        content.insert_str(adjusted.get(), text);
        if let Some((_, adj)) = offset_adjustments.last_mut() {
            adj.macro_name = name.map(|s| s.to_string());
            adj.original_location = original_loc;
        }
        Ok(info)
    }

    /// Replaces the source bytes at `original_range` (in the original, unmodified file) with
    /// `text`, and records the resulting length delta so that subsequent calls using offsets
    /// derived from the original source remain correct.
    ///
    /// Both range endpoints are translated through any previously recorded adjustments before the
    /// replacement is applied. Does nothing and returns `Ok(None)` if `original_range` is empty or
    /// inverted. Returns an error if byte-offset arithmetic overflows during adjustment
    /// computation.
    pub fn replace(self, original_range: Range<OriginalOffset>) -> eyre::Result<Option<EditInfo>> {
        let AdjustmentEntry { path, text, name, original_loc, sources, offset_adjustments } = self;
        if original_range.end <= original_range.start {
            return Ok(None);
        }
        let adjusted_start = offset_adjustments.adjusted_offset(path, original_range.start)?;
        let adjusted_end = offset_adjustments.adjusted_offset(path, original_range.end)?;
        let src = sources.get_mut(path).unwrap();
        let content = Arc::make_mut(&mut src.content);
        let removed = content[adjusted_start.get()..adjusted_end.get()].to_owned();
        let (_, info) = offset_adjustments.record(
            path,
            original_range.start,
            content.as_str(),
            text,
            &removed,
        )?;
        content.replace_range(adjusted_start.get()..adjusted_end.get(), text);
        if let Some((_, adj)) = offset_adjustments.last_mut() {
            adj.macro_name = name.map(|s| s.to_string());
            adj.original_location = original_loc;
        }
        Ok(Some(info))
    }
}

impl OffsetAdjustment {
    /// Returns the current offset in `path` corresponding to `original_offset` from the HIR,
    /// accounting for all length-changing edits recorded by previous macro rules. This is done
    /// by summing all offset deltas affecting the source code prior to the input `original_offset`.
    ///
    /// Returns an error if the accumulated delta overflows `isize`, or if the final adjusted
    /// offset is negative or exceeds `usize::MAX`.
    pub fn adjusted_offset(
        &self,
        path: &Path,
        original_offset: OriginalOffset,
    ) -> eyre::Result<ExpandedOffset> {
        let delta = self
            .iter()
            .filter(|(p, a)| p == path && a.original_offset <= original_offset)
            .try_fold(0isize, |acc, (_, a)| acc.checked_add(a.delta_offset))
            .ok_or_else(|| eyre::eyre!("offset delta overflow"))?;
        let raw = isize::try_from(original_offset.get())
            .ok()
            .and_then(|v| v.checked_add(delta))
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| eyre::eyre!("adjusted offset out of range"))?;
        Ok(ExpandedOffset::new(raw))
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
    /// Returns an error if any arithmetic overflows. See
    /// [`adjusted_offset`](Self::adjusted_offset).
    fn record(
        &mut self,
        path: &Path,
        original_offset: OriginalOffset,
        source: &str,
        added: &str,
        removed: &str,
    ) -> eyre::Result<(ExpandedOffset, EditInfo)> {
        let adjusted_offset = self.adjusted_offset(path, original_offset)?;
        let newline_count = source[..adjusted_offset.get()].bytes().filter(|&b| b == b'\n').count();
        let current_line = isize::try_from(newline_count)
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| eyre::eyre!("line count overflow"))?;
        let accumulated_line_delta = self
            .iter()
            .filter(|(p, a)| p.as_path() == path && a.original_offset <= original_offset)
            .try_fold(0isize, |acc, (_, a)| acc.checked_add(a.delta_line))
            .ok_or_else(|| eyre::eyre!("accumulated line delta overflow"))?;
        let original_line = current_line
            .checked_sub(accumulated_line_delta)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| eyre::eyre!("original line underflow"))?;
        let delta_offset = isize::try_from(added.len())
            .ok()
            .zip(isize::try_from(removed.len()).ok())
            .and_then(|(a, r)| a.checked_sub(r))
            .ok_or_else(|| eyre::eyre!("delta_offset overflow"))?;
        let delta_line = isize::try_from(added.bytes().filter(|&b| b == b'\n').count())
            .ok()
            .zip(isize::try_from(removed.bytes().filter(|&b| b == b'\n').count()).ok())
            .and_then(|(a, r)| a.checked_sub(r))
            .ok_or_else(|| eyre::eyre!("delta_line overflow"))?;
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
        Ok((adjusted_offset, EditInfo { expanded_line: current_line, delta_lines: delta_line }))
    }

    /// Returns the adjustment whose expanded byte range covers `expanded_offset` in `source`,
    /// if any.
    ///
    /// Adjustments are walked in insertion order. The accumulated byte delta is only applied when
    /// an adjustment's expanded position strictly precedes the query, which correctly handles
    /// out-of-order insertions (a later-recorded adjustment with a lower `original_offset`).
    ///
    /// Returns an error if any byte-offset arithmetic overflows. See
    /// [`adjusted_offset`](Self::adjusted_offset).
    pub fn find_macro_adjustment_by_offset(
        &self,
        source: &Path,
        expanded_offset: ExpandedOffset,
    ) -> eyre::Result<Option<&Adjustment>> {
        self.fold(source, expanded_offset, None, |pos, adj, acc| {
            let end = pos
                .checked_add(adj.added_len)
                .ok_or_else(|| eyre::eyre!("adjustment end offset overflow"))?;
            if pos <= expanded_offset && expanded_offset < end {
                *acc = Some(adj);
                Ok(ControlFlow::Break(()))
            } else {
                Ok(ControlFlow::Continue(()))
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
    ///
    /// Returns an error if any byte-offset arithmetic overflows. See
    /// [`adjusted_offset`](Self::adjusted_offset).
    pub fn get_original_offset(
        &self,
        source: &Path,
        expanded_offset: ExpandedOffset,
    ) -> eyre::Result<OriginalOffset> {
        self.fold(
            source,
            expanded_offset,
            OriginalOffset::new(expanded_offset.get()),
            |pos, adj, acc| {
                if pos < expanded_offset {
                    *acc = acc
                        .checked_sub_isize(adj.delta_offset)
                        .ok_or_else(|| eyre::eyre!("original offset underflow"))?;
                }
                Ok(ControlFlow::Continue(()))
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
    /// `f` returns `Ok(ControlFlow::Break(()))` to stop early, or an error to abort.
    fn fold<'a, V>(
        &'a self,
        source: &Path,
        expanded_offset: ExpandedOffset,
        mut acc: V,
        f: impl Fn(ExpandedOffset, &'a Adjustment, &mut V) -> eyre::Result<ControlFlow<()>>,
    ) -> eyre::Result<V> {
        let mut accumulated_delta = 0isize;
        for (_, adj) in self.iter().filter(|(p, _)| p == source) {
            let expanded_pos = isize::try_from(adj.original_offset.get())
                .ok()
                .and_then(|v| v.checked_add(accumulated_delta))
                .and_then(|v| usize::try_from(v).ok())
                .map(ExpandedOffset::new)
                .ok_or_else(|| eyre::eyre!("expanded position overflow"))?;
            match f(expanded_pos, adj, &mut acc)? {
                ControlFlow::Break(_) => return Ok(acc),
                ControlFlow::Continue(_) => {}
            }
            if expanded_pos < expanded_offset {
                accumulated_delta = accumulated_delta
                    .checked_add(adj.delta_offset)
                    .ok_or_else(|| eyre::eyre!("accumulated delta overflow"))?;
            }
        }
        Ok(acc)
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
        let (offset, _) = adj
            .record(
                Path::new("foo.sol"),
                OriginalOffset::new(16),
                src,
                "\nfunction baz() public {\n }\n",
                "",
            )
            .expect("Test failed");
        assert_eq!(offset.get(), 16);
        let mut modified = src.to_string();
        modified.insert_str(16, "\nfunction baz() public {\n }\n");
        let (offset, _) = adj
            .record(
                Path::new("foo.sol"),
                OriginalOffset::new(16),
                &modified,
                "\nfunction bingbong() public {\n }\n",
                "",
            )
            .expect("Test failed");
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
        let offset = adj
            .get_original_offset(Path::new("foo.sol"), ExpandedOffset::new(77))
            .expect("Test failed");
        assert_eq!(offset.get(), 16);
    }

    #[test]
    fn test_find_macro_adjustment_by_offset() {
        let adj = setup();
        // Byte 20 falls inside Edit 1's inserted range [16, 44).
        assert!(
            adj.find_macro_adjustment_by_offset(Path::new("foo.sol"), ExpandedOffset::new(20))
                .expect("Test failed")
                .is_some()
        );
        // Byte 77 is past both inserted ranges and belongs to original content.
        assert!(
            adj.find_macro_adjustment_by_offset(Path::new("foo.sol"), ExpandedOffset::new(77))
                .expect("Test failed")
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
        let (offset, _) = adj
            .record(Path::new("foo.sol"), OriginalOffset::new(15), src, added, removed)
            .expect("Test failed");
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
        assert_eq!(
            adj.adjusted_offset(Path::new("foo.sol"), OriginalOffset::new(41))
                .expect("Test failed")
                .get(),
            23
        );
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
        adj.record(Path::new("x.sol"), OriginalOffset::new(3), src, "AAA", "")
            .expect("Test failed");
        let mut after_a = src.to_string();
        after_a.insert_str(3, "AAA"); // "012AAA3456789"
        adj.record(Path::new("x.sol"), OriginalOffset::new(7), &after_a, "BBB", "")
            .expect("Test failed"); // expanded at 10

        // Expanded byte 8 is '5', which is original byte 5. Only A's delta applies.
        assert_eq!(
            adj.get_original_offset(Path::new("x.sol"), ExpandedOffset::new(8))
                .expect("Test failed")
                .get(),
            5
        );
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
        adj.record(Path::new("x.sol"), OriginalOffset::new(10), src, "AAA", "")
            .expect("Test failed");
        let mut after_a = src.to_string();
        after_a.insert_str(10, "AAA");
        adj.record(Path::new("x.sol"), OriginalOffset::new(2), &after_a, "BBB", "")
            .expect("Test failed");

        // Byte 4 falls inside B's expanded range [2, 5).
        assert!(
            adj.find_macro_adjustment_by_offset(Path::new("x.sol"), ExpandedOffset::new(4))
                .expect("Test failed")
                .is_some()
        );
        // Byte 5 is the first byte past B and before A — original content, not macro-generated.
        assert!(
            adj.find_macro_adjustment_by_offset(Path::new("x.sol"), ExpandedOffset::new(5))
                .expect("Test failed")
                .is_none()
        );
    }
}
