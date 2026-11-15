# Puts the original .mca region files back for a world converted to linear.
#
# For every dimension of the world, each .mca in mca-backup\<folder>\ is copied back into
# <dimension>\<folder>\ when that file is not already there. Nothing is deleted or overwritten,
# and the .linear files are left where they are (the server no longer reads them).
#
# Usage:
#   .\scripts\restore-mca.ps1 -World "C:\...\servers\rs-anarchy\world"
#   .\scripts\restore-mca.ps1 -World "..." -DryRun      # list what would be copied

param(
    [Parameter(Mandatory)][string]$World,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$folders = @("region", "entities", "poi")
$copied = 0
$skipped = 0

$dimensions = Get-ChildItem -Path (Join-Path $World "dimensions") -Directory -Recurse -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName "mca-backup") }

if (-not $dimensions) {
    Write-Host "No mca-backup folders under $World\dimensions: nothing to restore." -ForegroundColor Yellow
    return
}

foreach ($dimension in $dimensions) {
    foreach ($folder in $folders) {
        $source = Join-Path $dimension.FullName (Join-Path "mca-backup" $folder)
        if (-not (Test-Path $source)) { continue }
        $target = Join-Path $dimension.FullName $folder
        foreach ($file in Get-ChildItem -Path $source -Filter "*.mca") {
            $destination = Join-Path $target $file.Name
            if (Test-Path $destination) {
                $skipped++
                Write-Host "  kept (already there): $destination"
                continue
            }
            if ($DryRun) {
                Write-Host "  would copy: $($file.FullName) -> $destination"
            } else {
                New-Item -ItemType Directory -Path $target -Force | Out-Null
                Copy-Item -LiteralPath $file.FullName -Destination $destination
                Write-Host "  restored: $destination" -ForegroundColor Green
            }
            $copied++
        }
    }
}

$verb = if ($DryRun) { "would be restored" } else { "restored" }
Write-Host "$copied .mca file(s) $verb, $skipped already present." -ForegroundColor Cyan
Write-Host "The .linear files were left in place; the server does not read them after this revert." -ForegroundColor DarkGray
