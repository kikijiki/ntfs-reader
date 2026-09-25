//! The doc examples (README "Watching the journal", `docs/journal.md` "A monitor loop") are only
//! compile-checked, not run, so nothing catches a bad reason order: a temporary file created and
//! deleted through one handle is a single record with `FILE_CREATE | FILE_DELETE | CLOSE`, and
//! checking `FILE_CREATE` before `FILE_DELETE` reports it as created, never as deleted. These
//! tests extract each doc's reason-check order and classify synthetic record sequences with it.

use std::ffi::OsString;

use time::OffsetDateTime;

use crate::usn::{Reason, UsnRecord};

const README: &str = include_str!("../../../README.md");
const JOURNAL_GUIDE: &str = include_str!("../../../docs/journal.md");

/// What each reason a recipe tests for is called in its output.
const EVENTS: [(&str, Reason, &str); 5] = [
    ("FILE_DELETE", Reason::FILE_DELETE, "deleted"),
    ("RENAME_NEW_NAME", Reason::RENAME_NEW_NAME, "renamed"),
    ("FILE_CREATE", Reason::FILE_CREATE, "created"),
    ("DATA_OVERWRITE", Reason::DATA_OVERWRITE, "modified"),
    ("DATA_EXTEND", Reason::DATA_EXTEND, "modified"),
];

/// The reasons the first Rust code block after `marker` in `text` tests, in order of appearance.
/// The first of them must be `CLOSE` (the recipe acts on the records of the close only).
fn recipe_order(text: &str, marker: &str) -> Vec<String> {
    let after = &text[text
        .find(marker)
        .unwrap_or_else(|| panic!("the docs no longer contain {marker:?}"))..];
    let start = after
        .find("```rust")
        .unwrap_or_else(|| panic!("no Rust block after {marker:?}"));
    let block = &after[start..];
    let block = &block[..block[3..].find("```").expect("the block ends") + 3];
    let order: Vec<String> = block
        .split("Reason::")
        .skip(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                .collect()
        })
        .collect();
    assert_eq!(
        order.first().map(String::as_str),
        Some("CLOSE"),
        "the recipe after {marker:?} must act on the records of the close: {order:?}"
    );
    order
}

/// What a recipe that tests `order` (after `CLOSE`) says about `record`, or `None` for a record it
/// does not act on.
fn classify(order: &[String], record: &UsnRecord) -> Option<&'static str> {
    if !record.reason.contains(Reason::CLOSE) {
        return None;
    }
    order.iter().skip(1).find_map(|name| {
        let (_, reason, event) = EVENTS
            .iter()
            .find(|(known, ..)| known == name)
            .unwrap_or_else(|| panic!("the recipe tests {name}, which this test does not know"));
        record.reason.contains(*reason).then_some(*event)
    })
}

fn record(usn: i64, reason: Reason) -> UsnRecord {
    UsnRecord {
        usn,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        file_id: 0x0001_0000_0000_0100u64.into(),
        parent_id: 0x0005_0000_0000_0005u64.into(),
        reason,
        file_attributes: 0x20,
        name: OsString::from("file.txt"),
    }
}

/// What the recipe reports for a stream of records with these reasons.
fn events(order: &[String], reasons: &[Reason]) -> Vec<&'static str> {
    reasons
        .iter()
        .enumerate()
        .filter_map(|(index, &reason)| classify(order, &record(index as i64, reason)))
        .collect()
}

/// The sequences NTFS writes for one file, and what a monitor must report for each. A delete is
/// never lost, a temporary file is not reported as created, and a record without `CLOSE` is not
/// acted on (its bits come again in the close record).
fn assert_recipe_keeps_the_delete(order: &[String], what: &str) {
    let create = Reason::FILE_CREATE;
    let extend = Reason::DATA_EXTEND;
    let delete = Reason::FILE_DELETE;
    let close = Reason::CLOSE;
    let cases: [(&str, Vec<Reason>, Vec<&str>); 6] = [
        (
            "a temporary file created, written and deleted through one handle",
            vec![create | extend | delete | close],
            vec!["deleted"],
        ),
        (
            "the same with the intermediate records before the close",
            vec![create, create | extend, create | extend | delete | close],
            vec!["deleted"],
        ),
        (
            "a file that was modified and then deleted",
            vec![extend, extend | delete | close],
            vec!["deleted"],
        ),
        (
            "a file deleted with nothing else done to it",
            vec![delete | close],
            vec!["deleted"],
        ),
        (
            "a file created and closed",
            vec![create, create | extend | close],
            vec!["created"],
        ),
        (
            "a rename: the old-name record, then the new-name record with the close",
            vec![Reason::RENAME_OLD_NAME, Reason::RENAME_NEW_NAME | close],
            vec!["renamed"],
        ),
    ];
    for (case, reasons, expected) in cases {
        assert_eq!(events(order, &reasons), expected, "{what}: {case}");
    }
    assert_eq!(
        events(order, &[close]),
        Vec::<&str>::new(),
        "{what}: a close alone is nothing"
    );
}

#[test]
fn the_readme_loop_reports_the_delete_of_a_file_created_and_deleted_through_one_handle() {
    let order = recipe_order(README, "## Watching the journal");
    assert_recipe_keeps_the_delete(&order, "README");
}

#[test]
fn the_journal_guide_loop_reports_the_delete_of_a_file_created_and_deleted_through_one_handle() {
    let order = recipe_order(JOURNAL_GUIDE, "## A monitor loop");
    assert_recipe_keeps_the_delete(&order, "docs/journal.md");
}

// The check itself must be able to fail: a loop that asks about `FILE_CREATE` before `FILE_DELETE`
// reports the temporary file as created and loses the delete.
#[test]
fn a_recipe_that_tests_create_before_delete_loses_the_delete() {
    let text = "## Loop\n```rust\nif r.reason.contains(Reason::CLOSE) {\n    if reason.contains(Reason::FILE_CREATE) {\n    } else if reason.contains(Reason::FILE_DELETE) {\n    }\n}\n```\n";
    let order = recipe_order(text, "## Loop");
    assert_eq!(order, ["CLOSE", "FILE_CREATE", "FILE_DELETE"]);
    let temp = Reason::FILE_CREATE | Reason::DATA_EXTEND | Reason::FILE_DELETE | Reason::CLOSE;
    assert_eq!(events(&order, &[temp]), ["created"]);
    let caught = std::panic::catch_unwind(|| assert_recipe_keeps_the_delete(&order, "bad recipe"));
    assert!(
        caught.is_err(),
        "the check accepted a recipe that loses the delete"
    );
}
