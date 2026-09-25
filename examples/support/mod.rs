// Copyright (c) 2022, Matteo Bernacchia <dev@kikijiki.com>. All rights reserved.
// This project is dual licensed under the Apache License 2.0 and the MIT license.
// See the LICENSE files in the project root for details.

//! Small helpers the examples share (`mod support;`). Not part of the crate.
#![allow(dead_code)]

use std::error::Error;
use std::io::ErrorKind;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

/// Shared note for examples that take a VOLUME: a path other than the device path (`C:` or
/// `C:\`) fails with a bare "Access is denied", which looks like a missing elevation.
pub const VOLUME_HINT: &str = "VOLUME is the device path of the volume, `\\\\.\\C:`: no trailing backslash, and not `C:` or `C:\\`, which fail with \"Access is denied\" whether or not the shell is elevated.";

/// `text` with control characters written as `\u{..}`, so a name holding an escape sequence or
/// line break cannot rewrite the terminal or split a line. Everything else is unchanged,
/// backslashes and quotes included.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether `error` is a write to a closed pipe (`prog | head`): ends the program quietly instead
/// of panicking or printing an error.
pub fn is_broken_pipe(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == ErrorKind::BrokenPipe)
}

/// Whether a path from the journal goes through `$Extend\$Deleted`: Windows renames a file
/// deleted while open, or removed by `remove_dir_all`, into that directory first. Its delete
/// record then carries a random 24-hex-digit name under the live `$Deleted` parent, so the path
/// resolves to `\\.\C:\$Extend\$Deleted\<random>` and the real name is lost for good.
pub fn passes_through_deleted(path: &Path) -> bool {
    let names: Vec<_> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    names.windows(2).any(|pair| {
        pair[0].eq_ignore_ascii_case("$Extend") && pair[1].eq_ignore_ascii_case("$Deleted")
    })
}

/// When to stop for `--seconds`: `None` means run until interrupted, whether `--seconds` was
/// omitted or too large to fit an `Instant` (a plain add would panic).
pub fn deadline_after(seconds: Option<u64>) -> Option<Instant> {
    Instant::now().checked_add(Duration::from_secs(seconds?))
}

/// Whether `deadline` has passed.
pub fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_escaped_and_the_rest_is_left_alone() {
        assert_eq!(
            printable("a\u{1b}[31mred\nline\r"),
            "a\\u{1b}[31mred\\u{a}line\\u{d}"
        );
        assert_eq!(printable("\u{7f}\u{85}"), "\\u{7f}\\u{85}");
        let plain = "C:\\dir\\it's \"quoted\" \u{e9}\u{1f600}";
        assert_eq!(printable(plain), plain);
    }

    #[test]
    fn a_broken_pipe_is_recognised_through_the_boxed_error() {
        let pipe: Box<dyn Error> = std::io::Error::from(ErrorKind::BrokenPipe).into();
        assert!(is_broken_pipe(pipe.as_ref()));
        let other: Box<dyn Error> = std::io::Error::from(ErrorKind::NotFound).into();
        assert!(!is_broken_pipe(other.as_ref()));
        let text: Box<dyn Error> = "nope".into();
        assert!(!is_broken_pipe(text.as_ref()));
    }

    #[test]
    fn a_path_through_the_deleted_directory_is_a_lost_name() {
        let path = |parts: &[&str]| {
            parts
                .iter()
                .fold(Path::new(r"\\.\C:").to_path_buf(), |p, c| p.join(c))
        };
        assert!(passes_through_deleted(&path(&[
            "$Extend",
            "$Deleted",
            "02BD0000000002033008E7C5"
        ])));
        assert!(passes_through_deleted(&path(&[
            "$EXTEND", "$deleted", "x", "y"
        ])));
        assert!(!passes_through_deleted(&path(&["docs", "report.txt"])));
        assert!(!passes_through_deleted(&path(&["$Extend"])));
        assert!(!passes_through_deleted(&path(&["$Deleted", "$Extend"])));
        assert!(!passes_through_deleted(&path(&[
            "$Extend", "x", "$Deleted"
        ])));
    }

    #[test]
    fn a_huge_number_of_seconds_is_forever_and_not_a_panic() {
        assert!(deadline_after(None).is_none());
        assert!(deadline_after(Some(u64::MAX)).is_none());
        assert!(deadline_after(Some(5)).is_some());
        assert!(!expired(None));
        assert!(!expired(deadline_after(Some(3600))));
        assert!(expired(Instant::now().checked_sub(Duration::from_secs(1))));
    }
}
