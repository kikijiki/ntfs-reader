# Reading from a Volume Shadow Copy

A live volume changes while it is being read: `Mft::new` can catch a record half written, and
anything that reads `$MFT` more than once (a lazy or streaming reader, two separate scans) can
see two different states, torn records, an in-use flag and a `$BITMAP` bit set at different
moments, or an extension record caught mid-move. A Volume Shadow Copy (VSS) is Windows's answer:
a read-only block device frozen at one instant, with the file system consistent at that instant
(VSS flushes and holds writes while it freezes). Reading from one gives MFT and stream reads a
consistent point-in-time view.

Other guides: [deleted files](deleted-files.md), [paths and caches](paths-and-caches.md),
[reading file data](reading-data.md), [the journal](journal.md).

## Creating, finding and deleting one

The crate does not create shadow copies itself. On client Windows, creating a `ClientAccessible`
copy needs WMI or the COM `IVssBackupComponents` API and administrator rights. This PowerShell
example creates one, finds its device path for reading, then deletes it:

```powershell
# Create: 0.4 to 2 s measured. The first shadow of a volume also creates a 10% shadow-storage
# association on the volume by default, and the diff area (copy-on-write data) is written there
# unless you set one up elsewhere first (Win32_ShadowStorage.Create) - so making a shadow writes
# to the volume you are reading.
$result = Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create `
    -Arguments @{ Volume = "C:\"; Context = "ClientAccessible" }
if ($result.ReturnValue -ne 0) { throw "Win32_ShadowCopy.Create failed: $($result.ReturnValue)" }
$id = [string]$result.ShadowID

# Find: the device path Volume::new needs is DeviceObject, not the shadow ID.
$shadow = Get-CimInstance Win32_ShadowCopy -Filter "ID='$id'"
$devicePath = $shadow.DeviceObject   # \\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4

# ... open $devicePath with this crate, read what you need ...

# Delete: 10 to 12 ms measured. A shadow left behind keeps copy-on-write running on the volume
# until it is deleted, so delete it as soon as you are done reading.
Get-CimInstance Win32_ShadowCopy -Filter "ID='$id'" | Remove-CimInstance
```

`vssadmin delete shadows /shadow=<id>` is a fallback if `Remove-CimInstance` does not remove it.
Close everything the crate opened on the shadow (`Mft`, any open `StreamReader`) before deleting
it.

## Using it with the crate

`Volume::new` takes the device path exactly as any other volume path:

```rust,no_run
# use ntfs_reader::{Mft, Volume};
# fn main() -> Result<(), Box<dyn std::error::Error>> {
let volume = Volume::new(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4")?;
let mft = Mft::new(volume)?; // reads exactly as it would from a live volume
for file in mft.files().take(5) {
    println!("{}", file.number());
}
# Ok(())
# }
```

Shadow-copy reads have been verified on 32-bit and 64-bit Windows: `Volume::new` opens the
shadow, and `Mft::new` and `MftScan` agree on live and deleted files. File metadata, hard links,
named data streams (including a directory's stream) and resolved paths work through the shadow
device path.

`NtfsFile::open_stream` reads file data from the shadow, and `ClusterBitmap::new` with
`StreamReader::allocation` reports its cluster allocation. Verified cases include the extents
of a heavily fragmented 32 GiB file, a bounded read deep into it, and its allocation state.

`Journal::new` requires a live volume: the journal APIs read the live change journal, not a
frozen copy. Read the live volume's journal position before creating the shadow, load the
consistent base from the shadow, then follow the live journal from that position for changes
since the snapshot.

## Paths start with the device path

Every path the crate resolves is built on `Volume::path()`, and `Mft`/`Journal` reopen the volume
by that same path, so a file's path on a shadow starts with the shadow's device path, not a drive
letter:

```text
\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4\Windows\System32\ntdll.dll
```

This is usable as-is for reading (open another stream, walk to a parent), but it is not the path
Win32 would give you for the same file on the live volume. If you need that, build your own
mapping (replace the device-path prefix with the live volume's drive letter) once you know the two
name the same volume; the crate does not do this for you, since a shadow copy can outlive the
live path it was taken from meaning anything.

## Costs

- Administrator rights to create one (the same as opening any raw volume) and the VSS service
  (`VSS`, `swprv`) running.
- The volume needs shadow storage: the first shadow copy of a volume creates a default 10%
  association on itself, so creating one writes to the volume you are about to read.
- 0.4 to 2 s to create, 10 to 12 ms to delete in measurements.
- A shadow left behind keeps copy-on-write tracking running on the volume until it is deleted.
