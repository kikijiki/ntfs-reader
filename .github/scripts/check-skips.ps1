param(
    [Parameter(Mandatory)][string] $LogDirectory,
    [string] $JobStatus = 'success',
    # The architectures whose logs this job produced, comma separated (CI runs one test job per
    # architecture and passes its own).
    [string] $Architectures = 'x64,x86'
)

# Fixtures deliberately unavailable on the GitHub runner. Check names, including duplicates,
# so one unexpected skip cannot replace an expected one without failing the job.
$expected = @(
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

$failed = $false
foreach ($arch in @($Architectures -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })) {
    $log = Join-Path $LogDirectory "skips-$arch.txt"
    $lines = if (Test-Path $log) { @(Get-Content $log) } else { @() }
    $envSkips = @($lines | Where-Object { $_ -match '^\[env\]' })
    $skips = @($lines | Where-Object { $_ -notmatch '^\[env\]' })
    Write-Host "::group::$arch skips: $($skips.Count) (expected $($expected.Count)), environment skips: $($envSkips.Count)"
    $lines | ForEach-Object { Write-Host $_ }
    Write-Host '::endgroup::'
    # Failed jobs may have partial logs. Preserve the original failure in that case.
    if ($JobStatus -ne 'success') { continue }
    $names = @($skips | ForEach-Object {
        if ($_ -match '^SKIPPED: ([^:]+): ') { $Matches[1] } else { "Malformed skip: $_" }
    })
    $differences = @(Compare-Object -ReferenceObject $expected -DifferenceObject $names)
    foreach ($difference in $differences) {
        $kind = if ($difference.SideIndicator -eq '=>') { 'unexpected' } else { 'missing expected' }
        Write-Host "::error::${arch}: $kind skip: $($difference.InputObject)"
        $failed = $true
    }
}
if ($failed) { exit 1 }
