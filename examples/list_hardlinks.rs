use ntfs_reader::{DefaultPathCache, Mft, Volume};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let volume = Volume::new("\\\\.\\C:")?;
    let mft = Mft::new(volume)?;
    let mut cache = DefaultPathCache::new();

    for file in mft.files() {
        // Only files with more than one hard link are interesting here.
        if file.hard_links().nth(1).is_none() {
            continue;
        }

        println!("File record {}:", file.number());
        for link in file.hard_links() {
            match mft.resolve_path(&link, &mut cache) {
                Some(path) => println!("  {}", path.display()),
                None => println!("  {} (unresolved parent {})", link, link.parent_number()),
            }
        }
    }

    Ok(())
}
