# ntfs-reader

[![crates.io](https://img.shields.io/crates/v/ntfs-reader)](https://crates.io/crates/ntfs-reader)
[![docs.rs](https://img.shields.io/docsrs/ntfs-reader)](https://docs.rs/ntfs-reader)
![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/ntfs-reader)

## Features

- Fast in-memory scan of all records in the $MFT
- Usn journal reader

## Examples

See the `examples` directory for complete working examples.

## MFT Usage

```rust
// Open the C volume and its MFT.
// Must have elevated privileges or it will fail.
let volume = Volume::new("\\\\.\\C:")?;
let mft = Mft::new(volume)?;

// Iterate all files
for file in mft.files() {
    // Can also use FileInfo::with_cache().
    let info = FileInfo::new(&mft, &file);

    // Available fields: name, path, is_directory, size, file_attributes,
    // and timestamps (created, accessed, modified).
}

// Some perf comparison
// Type          Iteration  Drop       Total
// No Cache      12.326s    0          12.326s
// HashMap Cache 4.981s     323.150ms  5.305s
// Vec Cache     3.756s     114.670ms  3.871s
```

## Journal Usage

```rust
let volume = Volume::new("\\\\?\\C:")?;

// With `JournalOptions` you can customize things like where to start reading
// from (beginning, end, specific point), the mask to use for the events and more.
let mut journal = Journal::new(volume, JournalOptions::default())?;

// Try to read some events.
// You can call `read_sized` to use a custom buffer size.
for result in journal.read()? {
    // Available fields are: usn, timestamp, file_id, parent_id, reason, path.
}
```

## Development

On NixOS/Linux, enter the development shell directly or let direnv load it:

```sh
nix develop
# or: direnv allow

cargo xwin build --target x86_64-pc-windows-msvc
cargo xwin check --target i686-pc-windows-msvc
```

The flake includes Rust, both Windows MSVC Rust targets, `cargo-xwin`, and the
LLVM linker tools. Windows is still required to execute tests that access a raw
NTFS volume.

You can use plain cargo or install [mise](https://mise.jdx.dev/):

```sh
curl https://mise.run | sh
```

Tasks

```sh
mise fix      # Fix format and fixable linting errors
mise check    # Check format and linting issues
mise build    # Build debug
mise release  # Build release
mise test     # Run tests
mise test-32  # Run tests with the `i686-pc-windows-msvc` target, single threaded
mise bench    # Run benchmarks (slow!)
```
