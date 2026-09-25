use ntfs_reader::{DefaultPathCache, FileInfo, Mft, Volume};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let volume = Volume::new("\\\\.\\C:")?;
    let mft = Mft::new(volume)?;

    // A path cache remembers directory paths between files. It makes a full scan
    // much faster; `FileInfo::new(&file)` is for a single lookup.
    let mut cache = DefaultPathCache::new();

    for file in mft.files() {
        let info = FileInfo::with_cache(&file, &mut cache);

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
