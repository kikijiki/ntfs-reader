$ErrorActionPreference = 'Stop'
$checker = Join-Path $PSScriptRoot 'check-skips.ps1'
$logs = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $logs | Out-Null
try {
    # The ordinary runner has no fsutil opt-in, parity volume, large fixture or VSS shadow.
    $names = @(
        'a_deleted_file_nothing_has_touched_is_free_and_reads_intact'
        'a_partly_overwritten_deleted_file_is_partly_allocated'
        'a_deleted_file_whose_clusters_were_all_reused_is_allocated_and_reads_the_new_bytes'
        'a_deleted_file_after_a_trim_is_free_and_reads_as_zeroes'
        'a_deleted_file_reads_the_extents_sizes_and_bytes_it_had_while_live'
        'a_huge_fragmented_sparse_file_reads_like_win32_in_windows'
        'a_wof_compressed_file_refuses_its_default_stream'
        'compressed_and_encrypted_streams_are_refused'
        'the_parity_volume_is_smoke_checked'
        'mft_reads_a_shadow_copy_and_matches_the_known_fixtures'
        'open_stream_reads_a_fragmented_file_through_a_shadow_copy'
        'mft_scan_equals_mft_on_a_shadow_copy'
        'cluster_bitmap_reads_through_a_shadow_copy'
    )
    $valid = @($names | ForEach-Object { "SKIPPED: ${_}: fixture unavailable" })
    foreach ($arch in 'x64', 'x86') {
        $valid | Set-Content (Join-Path $logs "skips-$arch.txt")
    }
    function Check([string] $case, [int] $expectedExit, [string] $status = 'success', [string] $architectures = 'x64,x86') {
        $output = & pwsh -NoProfile -File $checker -LogDirectory $logs -JobStatus $status -Architectures $architectures
        if ($LASTEXITCODE -ne $expectedExit) {
            throw "${case}: expected exit $expectedExit, got $LASTEXITCODE`n$($output -join "`n")"
        }
        Write-Host "PASS $case"
    }
    Check 'all thirteen named skips on both architectures' 0
    $x86 = Join-Path $logs 'skips-x86.txt'
    '[env] SKIPPED (environment): trim: host lacks TRIM' | Add-Content $x86
    Check 'environment skips are separate' 0
    @($valid[0..11]) + 'SKIPPED: unexpected_test: unavailable' | Set-Content $x86
    Check 'same count with wrong name' 1
    @($valid[0..11]) + $valid[0] | Set-Content $x86
    Check 'duplicate replacing one expected name' 1
    @($valid[0..8]) | Set-Content $x86
    Check 'missing four shadow skips' 1
    Check 'partial log after a failed job' 0 'failure'
    Remove-Item $x86
    Check 'missing architecture log' 1
    Check 'one architecture checks only its own log' 0 'success' 'x64'
    Check 'one architecture still needs its own log' 1 'success' 'x86'
    Write-Host 'Skip policy tests passed'
} finally {
    Remove-Item -Recurse -Force $logs
}

# The last case runs check-skips.ps1 expecting exit 1; without this, its $LASTEXITCODE becomes the step's result.
exit 0
