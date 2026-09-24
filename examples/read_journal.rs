#[cfg(windows)]
use ntfs_reader::{Journal, JournalOptions, NextUsn, Volume};

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open the C: volume
    let volume = Volume::new("\\\\?\\C:")?;

    // With `JournalOptions` you can customize things like where to start reading
    // from (beginning, end, specific point), the mask to use for the events and more.
    let options = JournalOptions {
        // Start from the beginning of the journal.
        // Normally you'd use the default NextUsn::Next to read from the current position.
        next_usn: NextUsn::First,
        ..Default::default()
    };
    let mut journal = Journal::new(volume, options)?;

    // Try to read some events. One read() call returns one page of the journal, not every
    // event since `next_usn`; call it in a loop and use `caught_up` to know when to stop.
    // You can call `read_sized(buffer_size)` to use a custom buffer size.
    let result = journal.read()?;

    println!(
        "Read {} journal events from this page (caught up: {})",
        result.records.len(),
        result.caught_up
    );

    for event in result.records.iter().take(10) {
        // Available fields (public fields, not methods)
        // usn, timestamp, file_id, parent_id, reason, file_attributes, name

        // event.name is the file name only, as a lossless OsString. Resolving the full path
        // is a separate, explicit step: it costs one or two handle opens per call, so it's
        // not done for every record read() returns. It is None when the file and its parent
        // are both gone (for example after a delete).
        let path = journal.resolve_path(event);

        // Example: Print information for each journal event
        println!(
            "USN: {}, Time: {}, Path: {}, Reason: {}",
            event.usn,
            event.timestamp,
            path.as_deref()
                .map_or_else(|| "(unresolved)".into(), |p| p.display().to_string()),
            event.reason
        );
    }

    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "read_journal: windows-only (Journal needs DeviceIoControl); skipped on this platform"
    );
}
