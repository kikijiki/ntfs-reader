use ntfs_reader::file::NtfsNameKind;
use ntfs_reader::mft::Mft;
use ntfs_reader::volume::Volume;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open the C: volume
    let volume = Volume::new("\\\\.\\C:")?;
    let mft = Mft::new(volume)?;

    for file in mft.files() {
        let names = file.all_file_names(&mft);
        let link_count = names
            .iter()
            .filter(|entry| entry.kind == NtfsNameKind::Link)
            .count();

        // Only files with more than one hard link are interesting here.
        if link_count < 2 {
            continue;
        }

        println!(
            "File record {} has {} hard links:",
            file.number(),
            link_count
        );
        for entry in &names {
            match entry.kind {
                NtfsNameKind::Link => {
                    println!("  {} (parent record {})", entry.name, entry.name.parent())
                }
                NtfsNameKind::DosAlias => println!("  {} (short-name alias)", entry.name),
            }
        }
    }

    Ok(())
}
