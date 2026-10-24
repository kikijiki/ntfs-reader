# Reading from a Volume Shadow Copy

A live volume changes while you read it. `Mft::new` can catch a record half written, and anything
that reads `$MFT` more than once can see two different states:

- torn records
- an in-use flag and a `$BITMAP` bit set at different moments
- an extension record caught mid-move

A Volume Shadow Copy (VSS) is a read-only block device frozen at one instant, with a consistent file
system. Reading from one gives MFT and stream reads a point-in-time view.

Other guides: [deleted files](deleted-files.md), [paths and caches](paths-and-caches.md),
[reading file data](reading-data.md), [the journal](journal.md).

## Creating, finding and deleting one

The crate doesn't create shadow copies. On client Windows, creating a `ClientAccessible` copy needs
WMI or the COM `IVssBackupComponents` API, plus administrator rights. This PowerShell creates one,
finds its device path and deletes it:

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

- If `Remove-CimInstance` doesn't remove it, use `vssadmin delete shadows /shadow=<id>`.
- Close everything the crate opened on the shadow (`Mft`, open `StreamReader`s) before deleting it.

## Using it with the crate

`Volume::new` takes the device path like any other volume path:

```rust,no_run
use ntfs_reader::{Mft, Volume};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let volume = Volume::new(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4")?;
    let mft = Mft::new(volume)?; // reads exactly as it would from a live volume
    for file in mft.files().take(5) {
        println!("{}", file.number());
    }
    Ok(())
}
```

Verified on 32-bit and 64-bit Windows:

- `Volume::new` opens the shadow, and `Mft::new` and `MftScan` agree on live and deleted files.
- File metadata, hard links, named data streams (including a directory's) and resolved paths work.
- `open_stream` reads file data, and `ClusterBitmap::new` with `StreamReader::allocation` reports
  cluster allocation. This was checked on a heavily fragmented 32 GiB file: its extents, a bounded
  read deep into it, and its allocation state.

`Journal::new` needs a live volume, because it reads the live change journal. To follow changes since
the snapshot:

1. Read the live volume's journal position.
2. Create the shadow.
3. Load the base from the shadow.
4. Follow the live journal from the saved position.

## Paths start with the device path

Paths are built on `Volume::path()`, and `Mft` and `Journal` reopen the volume by that path. On a
shadow, a file's path starts with the device path, not a drive letter:

```text
\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy4\Windows\System32\ntdll.dll
```

This works for reading, but it isn't the path Win32 gives for the same file on the live volume. If
you need that, replace the prefix with the live drive letter yourself. The crate doesn't, because a
shadow copy can outlive the live path it was taken from.

## Costs

- Administrator rights, and the VSS service (`VSS`, `swprv`) running.
- The volume needs shadow storage. The first shadow of a volume creates a default 10% association
  on itself, so creating one writes to the volume you are about to read.
- Creating one took 0.4 to 2 s, and deleting it 10 to 12 ms.
- A shadow left behind keeps copy-on-write tracking running on the volume until it is deleted.
