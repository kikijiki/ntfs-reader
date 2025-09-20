use ntfs_reader::journal::{Journal, JournalOptions, NextUsn};
use ntfs_reader::volume::Volume;

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

    // Try to read some events.
    // You can call `read_sized` to use a custom buffer size.
    let events = journal.read()?;

    println!("Found {} journal events", events.len());

    for event in events.iter().take(10) {
        // Available fields (public fields, not methods)
        // usn, timestamp, file_id, parent_id, reason, path

        // Example: Print information for each journal event
        println!(
            "USN: {}, Time: {:?}, Path: {}, Reason: {}",
            event.usn,
            event.timestamp,
            event.path.display(),
            Journal::get_reason_str(event.reason)
        );
    }

    Ok(())
}
