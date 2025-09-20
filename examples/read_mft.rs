use ntfs_reader::file_info::FileInfo;
use ntfs_reader::mft::Mft;
use ntfs_reader::volume::Volume;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open the C: volume
    let volume = Volume::new("\\\\.\\C:")?;
    let mft = Mft::new(volume)?;

    // Iterate all files
    mft.iterate_files(|file| {
        // Can also use FileInfo::with_cache() for better performance with repeated lookups
        let info = FileInfo::new(&mft, file);

        // Example: Print information for each file
        // Available fields: name, path, is_directory, size, created, accessed, modified
        println!(
            "Path: {}, Size: {} bytes, Directory: {}",
            info.path.display(),
            info.size,
            info.is_directory
        );
    });

    Ok(())
}
