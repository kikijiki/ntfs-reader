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

// FileInfo/get_best_file_name return only one name. List every hard link
// (and DOS short-name alias) instead:
for entry in file.all_file_names(&mft) {
    match entry.kind {
        NtfsNameKind::Link => println!("{} (parent {})", entry.name, entry.name.parent()),
        NtfsNameKind::DosAlias => println!("{} (short-name alias)", entry.name),
    }
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

On x86_64 Linux, the same development shell includes the project's QEMU Windows
VM launcher. Its mutable disks and installation media live outside the checkout
in `$NTFS_READER_VM_DIR` (by default `~/.local/share/windows-vm`):

```sh
ntfs-windows-vm start
ntfs-windows-vm status
ntfs-windows-vm view
ntfs-windows-vm stop
```

You can also start it without entering the shell with
`nix run .#windows-vm -- start`.

You can use plain cargo or install [mise](https://mise.jdx.dev/):

```sh
curl https://mise.run | sh
```

Tasks

On Windows these run Cargo natively. On other platforms they enter the Nix
development shell and cross-compile for Windows MSVC.

```sh
mise fix      # Fix format and fixable linting errors
mise check    # Check format and linting issues
mise build    # Build debug
mise release  # Build release
mise test     # Run tests on Windows; compile them for MSVC elsewhere
mise test-32  # Run 32-bit tests on Windows; check all targets elsewhere
mise bench    # Run benchmarks on Windows; compile them elsewhere
mise publish -n # Verify the crate package without uploading
```
