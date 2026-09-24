use ntfs_reader::{DefaultPathCache, FileInfo, Mft, Volume};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open the C: volume
    let volume = Volume::new("\\\\.\\C:")?;
    let mft = Mft::new(volume)?;

    // A path cache remembers directory paths between files. It makes a full scan
    // about twice as fast; `FileInfo::new(&file)` is for a single lookup.
    let mut cache = DefaultPathCache::new();

    // Iterate all files
    for file in mft.files() {
        let info = FileInfo::with_cache(&file, &mut cache);

        // Example: Print information for each file
        // Available fields: name, path, is_directory, size, file_attributes,
        // created, accessed, modified. `path` is None if it cannot be resolved.
        println!(
            "Path: {}, Size: {} bytes, Directory: {}",
            info.path.as_deref().map_or_else(
                || "<unresolved>".to_string(),
                |path| path.display().to_string()
            ),
            info.size,
            info.is_directory
        );
    }

    Ok(())
}
