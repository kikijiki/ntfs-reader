# ntfs-reader

[![crates.io](https://img.shields.io/crates/v/ntfs-reader)](https://crates.io/crates/ntfs-reader)
[![docs.rs](https://img.shields.io/docsrs/ntfs-reader)](https://docs.rs/ntfs-reader)
![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/ntfs-reader)

## Features

- Fast in-memory scan of all records in the $MFT
- Usn journal reader

## MFT Usage

```rust
// Open the C volume and its MFT.
// Must have elevated privileges or it will fail.
let volume = Volume::new("\\\\.\\C:")?;
let mft = Mft::new(volume)?;

// Iterate all files
mft.iterate_files(|file| {
    // Can also use FileInfo::with_cache().
    let info = FileInfo::new(mft, file);

    // Available fields: name, path, is_directory, size, timestamps (created, accessed, modified).
});

// Some perf comparison
// Type          Iteration  Drop       Total
// No Cache      12.326s    0          12.326s
// HashMap Cache 4.981s     323.150ms  5.305s
// Vec Cache     3.756s     114.670ms  3.871s
```

## Journal Usage

```rust
let volume = Volume::new("\\\\?\\C:")?;

// With `JournalOptions` you can customize things like where to start reading from (beginning, end, specific point),
// the mask to use for the events and more.
let journal = Journal::new(volume, JournalOptions::default())?;

// Try to read some events.
// You can call `read_sized` to use a custom buffer size.
for result in journal.read()? {
    // Available fields are: usn, timestamp, file_id, parent_id, reason, path.
}
```
