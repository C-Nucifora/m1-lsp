//! Single-splice semantic-token delta (#231).
//!
//! LSP `semanticTokens/full/delta` edits operate on the FLATTENED `u32` data
//! array (five integers per token). A typical edit touches one contiguous
//! region of the file, so one splice — the common prefix and suffix trimmed
//! off — captures it; the worst case degrades to one edit replacing
//! everything, which is exactly the full response in delta clothing.
use tower_lsp::lsp_types::{SemanticToken, SemanticTokensEdit};

/// The minimal single-splice edit list turning `prev` into `next`, computed
/// at TOKEN granularity (always a valid 5-integer-aligned splice) with
/// `start`/`delete_count` expressed in flattened-integer units as the wire
/// format requires. Empty when the streams are identical.
pub fn single_splice_edit(
    prev: &[SemanticToken],
    next: &[SemanticToken],
) -> Vec<SemanticTokensEdit> {
    let mut start = 0usize;
    while start < prev.len() && start < next.len() && prev[start] == next[start] {
        start += 1;
    }
    if start == prev.len() && start == next.len() {
        return Vec::new(); // identical
    }
    let mut suffix = 0usize;
    while suffix < prev.len() - start
        && suffix < next.len() - start
        && prev[prev.len() - 1 - suffix] == next[next.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let delete_tokens = prev.len() - start - suffix;
    let data: Vec<SemanticToken> = next[start..next.len() - suffix].to_vec();
    vec![SemanticTokensEdit {
        start: (start * 5) as u32,
        delete_count: (delete_tokens * 5) as u32,
        data: if data.is_empty() { None } else { Some(data) },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(dl: u32, ds: u32, len: u32, ty: u32) -> SemanticToken {
        SemanticToken {
            delta_line: dl,
            delta_start: ds,
            length: len,
            token_type: ty,
            token_modifiers_bitset: 0,
        }
    }

    #[test]
    fn identical_streams_need_no_edit() {
        let t = vec![tok(0, 0, 3, 1), tok(1, 0, 2, 2)];
        assert!(single_splice_edit(&t, &t).is_empty());
    }

    #[test]
    fn middle_change_is_one_tight_splice() {
        let a = vec![tok(0, 0, 3, 1), tok(1, 0, 2, 2), tok(1, 0, 4, 3)];
        let b = vec![tok(0, 0, 3, 1), tok(1, 0, 9, 2), tok(1, 0, 4, 3)];
        let edits = single_splice_edit(&a, &b);
        assert_eq!(edits.len(), 1);
        // The middle token is replaced: token index 1 → flattened start 5.
        assert_eq!(edits[0].start, 5);
        assert_eq!(edits[0].delete_count, 5);
        assert_eq!(edits[0].data.as_deref(), Some(&[tok(1, 0, 9, 2)][..]));
    }

    #[test]
    fn append_and_truncate_work() {
        let a = vec![tok(0, 0, 3, 1)];
        let b = vec![tok(0, 0, 3, 1), tok(1, 0, 2, 2)];
        let e = single_splice_edit(&a, &b);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].start, 5);
        assert_eq!(e[0].delete_count, 0);
        assert_eq!(e[0].data.as_ref().unwrap().len(), 1);

        let e2 = single_splice_edit(&b, &a);
        assert_eq!(e2[0].start, 5);
        assert_eq!(e2[0].delete_count, 5);
        assert!(e2[0].data.is_none());
    }

    #[test]
    fn disjoint_lengths_with_shared_affixes() {
        // Prefix/suffix overlap must not double-count when streams share
        // content (start bound respected).
        let a = vec![tok(0, 0, 1, 1), tok(0, 2, 1, 1)];
        let b = vec![tok(0, 0, 1, 1)];
        let e = single_splice_edit(&a, &b);
        assert_eq!(e.len(), 1);
        let removed = e[0].delete_count as usize;
        let added = e[0].data.as_deref().map_or(0, |d| d.len() * 5);
        assert_eq!(10 - removed + added, 5, "edit transforms a into b");
    }
}
