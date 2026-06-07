//! textDocument/documentLink: hyperlink `Filename="…"` attribute values in a
//! `Project.m1prj` to the backing script they name, so the editor can underline
//! and Ctrl/⌘-click through to the file (#175).
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::contained_join;
use std::path::Path;
use tower_lsp::lsp_types::{DocumentLink, Range, Url};

/// Document links for every `Filename="<path>"` in `text` whose value resolves to
/// a file contained within `root` (the project directory). Out-of-tree or
/// absolute filenames are skipped — the same containment rule goto uses (#134).
pub fn document_links(
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    root: &Path,
) -> Vec<DocumentLink> {
    const NEEDLE: &str = "Filename=\"";
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = text[search..].find(NEEDLE) {
        let val_start = search + rel + NEEDLE.len();
        let Some(end_rel) = text[val_start..].find('"') else {
            break;
        };
        let val_end = val_start + end_rel;
        let filename = &text[val_start..val_end];
        search = val_end + 1;
        if filename.is_empty() {
            continue;
        }
        let Some(target_path) = contained_join(root, filename) else {
            continue;
        };
        let Ok(target) = Url::from_file_path(&target_path) else {
            continue;
        };
        out.push(DocumentLink {
            range: Range::new(li.position(val_start, enc), li.position(val_end, enc)),
            target: Some(target),
            tooltip: Some(format!("Open {filename}")),
            data: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_filename_attribute_to_backing_script() {
        let root = std::path::Path::new("/proj");
        let text =
            "<Project>\n  <Component Filename=\"Scripts/Demo.Compute.m1scr\"/>\n</Project>\n";
        let li = LineIndex::new(text);
        let links = document_links(text, &li, PositionEncoding::Utf16, root);
        assert_eq!(links.len(), 1, "one Filename link");
        let l = &links[0];
        assert!(
            l.target
                .as_ref()
                .unwrap()
                .to_file_path()
                .unwrap()
                .ends_with("Scripts/Demo.Compute.m1scr"),
            "target: {:?}",
            l.target
        );
        // Range covers the value text, on line 1.
        assert_eq!(l.range.start.line, 1);
        assert!(l.range.end.character > l.range.start.character);
    }

    #[test]
    fn skips_out_of_tree_filename() {
        let root = std::path::Path::new("/proj");
        let text = "<Component Filename=\"../../etc/passwd\"/>\n";
        let li = LineIndex::new(text);
        assert!(document_links(text, &li, PositionEncoding::Utf16, root).is_empty());
    }
}
