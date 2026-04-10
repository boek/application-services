/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Import bookmarks from a Netscape Bookmark File (HTML).
//!
//! The Netscape Bookmark File format is the de-facto HTML export format used by
//! Chrome, Safari, Firefox desktop, and most other browsers. It encodes bookmarks
//! as `<A>` tags and folders as `<H3>` tags with nested `<DL>` lists.
//!
//! The import preserves the folder hierarchy from the source file and places all
//! top-level items under `import_root_guid`. Every inserted record gets
//! `syncStatus = New` and `syncChangeCounter = 1` so it is uploaded on the next
//! sync cycle (handled automatically by `insert_bookmark`).

use std::time::Instant;

use scraper::{ElementRef, Html};
use sync_guid::Guid as SyncGuid;
use types::Timestamp;
use url::Url;

use crate::error::Result;
use crate::import::common::{BookmarksMigrationResult, NOW};
use crate::storage::bookmarks::{
    insert_bookmark, BookmarkPosition, InsertableBookmark, InsertableFolder, InsertableItem,
    InsertableSeparator,
};
use crate::storage::URL_LENGTH_MAX;
use crate::PlacesDb;

pub fn import(
    conn: &PlacesDb,
    html_path: &str,
    import_root_guid: &SyncGuid,
) -> Result<BookmarksMigrationResult> {
    let start = Instant::now();

    let html_content = std::fs::read_to_string(html_path)?;
    let document = Html::parse_document(&html_content);

    // Find the first top-level <dl> — the root of the bookmarks tree.
    let dl_selector = scraper::Selector::parse("dl").expect("valid selector");
    let root_dl = document.select(&dl_selector).next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Not a valid Netscape Bookmark File: no <dl> element found",
        )
    })?;

    let mut total = 0u64;
    let mut failed = 0u64;

    let top_level_items = parse_dl(root_dl, import_root_guid, &mut total, &mut failed);

    let mut succeeded = 0u64;
    for item in top_level_items {
        let item_count = count_items(&item);
        match insert_bookmark(conn, item) {
            Ok(_) => succeeded += item_count,
            Err(e) => {
                crate::error::warn!("Failed to insert bookmark during HTML import: {}", e);
                failed += item_count;
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok(BookmarksMigrationResult {
        total,
        succeeded,
        failed,
        duration_ms,
    })
}

/// Recursively walk a `<dl>` element and build `InsertableItem`s.
///
/// In the Netscape format, `<dt>` and the following sibling `<dl>` (the folder's
/// children) are at the same level under the parent `<dl>`. We handle this by
/// keeping a `pending_folder` that is "resolved" when we encounter the sibling
/// `<dl>`.
///
/// Children of folders receive `parent_guid = SyncGuid::from("")`. This is a
/// special sentinel value that `insert_bookmark_in_tx` replaces with the actual
/// parent GUID during recursive insertion (see `storage/bookmarks.rs`).
fn parse_dl(
    dl: ElementRef<'_>,
    parent_guid: &SyncGuid,
    total: &mut u64,
    failed: &mut u64,
) -> Vec<InsertableItem> {
    let mut items: Vec<InsertableItem> = Vec::new();
    let mut pending_folder: Option<(Option<String>, Option<Timestamp>, Option<Timestamp>)> = None;
    // Empty GUID sentinel — insert_bookmark_in_tx replaces it with the real parent GUID.
    let empty_guid = SyncGuid::from("");

    for child in dl.children().filter_map(ElementRef::wrap) {
        match child.value().name() {
            "dt" => {
                // Flush any pending folder whose children <dl> never arrived.
                if let Some((title, date_added, last_modified)) = pending_folder.take() {
                    *total += 1;
                    items.push(make_folder(
                        parent_guid.clone(),
                        title,
                        date_added,
                        last_modified,
                        vec![],
                    ));
                }

                let Some(inner) = first_child_element(child) else {
                    continue;
                };
                match inner.value().name() {
                    "a" => {
                        *total += 1;
                        if let Some(bm) = parse_bookmark(inner, parent_guid) {
                            items.push(InsertableItem::Bookmark { b: bm });
                        } else {
                            *failed += 1;
                        }
                    }
                    "h3" => {
                        let title = collect_text(inner);
                        let date_added = parse_timestamp(inner.value().attr("add_date"));
                        let last_modified = parse_timestamp(inner.value().attr("last_modified"));
                        // In the HTML5-parsed Netscape format, the folder's children
                        // <dl> appears as a sibling of <h3> inside the same <dt>
                        // (not as a sibling of <dt> in the parent <dl>), because
                        // HTML5 parsing doesn't close an open <dt> when it sees <dl>.
                        let opt_children_dl = child
                            .children()
                            .filter_map(ElementRef::wrap)
                            .find(|el| el.value().name() == "dl");
                        if let Some(children_dl) = opt_children_dl {
                            let children = parse_dl(children_dl, &empty_guid, total, failed);
                            *total += 1;
                            items.push(make_folder(
                                parent_guid.clone(),
                                title,
                                date_added,
                                last_modified,
                                children,
                            ));
                        } else {
                            // No <dl> inside this <dt>; fall back to checking for a
                            // sibling <dl> at the parent <dl> level.
                            pending_folder = Some((title, date_added, last_modified));
                        }
                    }
                    "hr" => {
                        *total += 1;
                        items.push(InsertableItem::Separator {
                            s: InsertableSeparator {
                                parent_guid: parent_guid.clone(),
                                position: BookmarkPosition::Append,
                                date_added: None,
                                last_modified: None,
                                guid: None,
                            },
                        });
                    }
                    _ => {}
                }
            }
            "dl" => {
                // This <dl> holds the children of the preceding <h3> folder.
                let (title, date_added, last_modified) =
                    pending_folder.take().unwrap_or((None, None, None));
                let children = parse_dl(child, &empty_guid, total, failed);
                *total += 1;
                items.push(make_folder(
                    parent_guid.clone(),
                    title,
                    date_added,
                    last_modified,
                    children,
                ));
            }
            _ => {}
        }
    }

    // Flush any trailing pending folder (no children <dl> followed it).
    if let Some((title, date_added, last_modified)) = pending_folder.take() {
        *total += 1;
        items.push(make_folder(
            parent_guid.clone(),
            title,
            date_added,
            last_modified,
            vec![],
        ));
    }

    items
}

fn make_folder(
    parent_guid: SyncGuid,
    title: Option<String>,
    date_added: Option<Timestamp>,
    last_modified: Option<Timestamp>,
    children: Vec<InsertableItem>,
) -> InsertableItem {
    InsertableItem::Folder {
        f: InsertableFolder {
            parent_guid,
            position: BookmarkPosition::Append,
            date_added,
            last_modified,
            guid: None,
            title,
            children,
        },
    }
}

fn parse_bookmark(a: ElementRef<'_>, parent_guid: &SyncGuid) -> Option<InsertableBookmark> {
    let href = a.value().attr("href")?;
    if href.len() > URL_LENGTH_MAX {
        return None;
    }
    let url = Url::parse(href).ok()?;
    Some(InsertableBookmark {
        parent_guid: parent_guid.clone(),
        position: BookmarkPosition::Append,
        date_added: parse_timestamp(a.value().attr("add_date")),
        last_modified: parse_timestamp(a.value().attr("last_modified")),
        guid: None,
        url,
        title: collect_text(a),
    })
}

/// Collect all text content from an element (strips any child HTML tags).
fn collect_text(el: ElementRef<'_>) -> Option<String> {
    let text: String = el.text().collect();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Parse a Netscape Bookmark File timestamp (Unix seconds) into a `Timestamp`
/// (milliseconds). Returns `None` for missing, unparseable, or out-of-range values.
fn parse_timestamp(s: Option<&str>) -> Option<Timestamp> {
    let seconds: i64 = s?.parse().ok()?;
    let millis = seconds.checked_mul(1000)?;
    let ts = Timestamp(u64::try_from(millis).ok()?);
    let now = *NOW;
    if Timestamp::EARLIEST <= ts && ts <= now {
        Some(ts)
    } else {
        None
    }
}

fn first_child_element(el: ElementRef<'_>) -> Option<ElementRef<'_>> {
    el.children().filter_map(ElementRef::wrap).next()
}

/// Count the total number of items in an `InsertableItem` tree (including the
/// item itself and all descendants).
fn count_items(item: &InsertableItem) -> u64 {
    match item {
        InsertableItem::Bookmark { .. } | InsertableItem::Separator { .. } => 1,
        InsertableItem::Folder { f } => 1 + f.children.iter().map(count_items).sum::<u64>(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::bookmarks::BookmarkRootGuid;
    use crate::ConnectionType;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn new_mem_db() -> PlacesDb {
        PlacesDb::open_in_memory(ConnectionType::ReadWrite).expect("memory db")
    }

    fn write_html(html: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(html.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_single_bookmark() {
        let conn = new_mem_db();
        let f = write_html(
            r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<H1>Bookmarks</H1>
<DL><p>
    <DT><A HREF="https://example.com" ADD_DATE="1234567890">Example</A>
</DL>"#,
        );
        let guid = BookmarkRootGuid::Unfiled.as_guid();
        let result = import(&conn, f.path().to_str().unwrap(), &guid).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.succeeded, 1);
        assert_eq!(result.failed, 0);
    }

    #[test]
    fn test_invalid_url_counted_as_failed() {
        let conn = new_mem_db();
        let f = write_html(
            r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<DL><p>
    <DT><A HREF="not a url">Bad</A>
    <DT><A HREF="https://good.example.com">Good</A>
</DL>"#,
        );
        let guid = BookmarkRootGuid::Unfiled.as_guid();
        let result = import(&conn, f.path().to_str().unwrap(), &guid).unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(result.succeeded, 1);
        assert_eq!(result.failed, 1);
    }

    #[test]
    fn test_nested_folder() {
        let conn = new_mem_db();
        let f = write_html(
            r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<DL><p>
    <DT><H3 ADD_DATE="1234567890">My Folder</H3>
    <DL><p>
        <DT><A HREF="https://child.example.com">Child</A>
    </DL><p>
</DL>"#,
        );
        let guid = BookmarkRootGuid::Unfiled.as_guid();
        let result = import(&conn, f.path().to_str().unwrap(), &guid).unwrap();
        // 1 folder + 1 bookmark = 2 items
        assert_eq!(result.total, 2);
        assert_eq!(result.succeeded, 2);
        assert_eq!(result.failed, 0);
    }

    #[test]
    fn test_separator() {
        let conn = new_mem_db();
        let f = write_html(
            r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<DL><p>
    <DT><A HREF="https://before.example.com">Before</A>
    <DT><HR>
    <DT><A HREF="https://after.example.com">After</A>
</DL>"#,
        );
        let guid = BookmarkRootGuid::Unfiled.as_guid();
        let result = import(&conn, f.path().to_str().unwrap(), &guid).unwrap();
        assert_eq!(result.total, 3);
        assert_eq!(result.succeeded, 3);
        assert_eq!(result.failed, 0);
    }

    #[test]
    fn test_empty_bookmarks_file() {
        let conn = new_mem_db();
        let f = write_html(
            r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<DL><p>
</DL>"#,
        );
        let guid = BookmarkRootGuid::Unfiled.as_guid();
        let result = import(&conn, f.path().to_str().unwrap(), &guid).unwrap();
        assert_eq!(result.total, 0);
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 0);
    }

    #[test]
    fn test_not_a_bookmark_file_returns_error() {
        let conn = new_mem_db();
        let f = write_html("<html><body><p>No bookmarks here</p></body></html>");
        let guid = BookmarkRootGuid::Unfiled.as_guid();
        assert!(import(&conn, f.path().to_str().unwrap(), &guid).is_err());
    }

    #[test]
    fn test_nonexistent_path_returns_error() {
        let conn = new_mem_db();
        let guid = BookmarkRootGuid::Unfiled.as_guid();
        assert!(import(&conn, "/does/not/exist.html", &guid).is_err());
    }

    /// End-to-end test using a large generated Netscape Bookmark File fixture.
    ///
    /// The fixture contains folders and bookmarks across many categories.
    /// 976 bookmarks have underscores in their hostnames
    /// (e.g. `www.3d_printing__beginner_guide.example.com`), which are
    /// legitimately invalid per the WHATWG URL Standard / IDNA rules — the
    /// url crate correctly rejects them, so they are counted as `failed`.
    #[test]
    fn test_import_firefox_export() {
        let conn = new_mem_db();
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test-fixtures/example_bookmarks.html"
        );
        let guid = BookmarkRootGuid::Mobile.as_guid();
        let result = import(&conn, fixture, &guid).unwrap();

        assert_eq!(result.total, 11280, "total items");
        assert_eq!(result.succeeded, 10304, "succeeded");
        // 976 bookmarks have underscores in their hostnames and fail URL validation
        assert_eq!(result.failed, 976, "failed");
        assert!(result.duration_ms < 5000, "should complete quickly");
    }
}
