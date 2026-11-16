<#
.SYNOPSIS
    Puts the backed-up original .mca region files back into a world converted to linear.

.DESCRIPTION
    The linear converter moved each original .mca into <dimension>\mca-backup\<folder>\.
    The current .mca files in region, entities and poi are moved aside into a timestamped
    mca-replaced-<timestamp> folder, then every backed-up .mca is copied back in their place.

    Safety:
    - Refuses to run while the server is running (its processes, or the world's session.lock).
    - Never deletes: current .mca files are moved to mca-replaced-<timestamp>\<folder>\ first.
    - Leaves the .linear files and mca-backup\ untouched.
    - Writes a log next to the dimensions folder.

.PARAMETER Dimensions
    The world's dimensions folder. Defaults to the rs-anarchy server's main world.

.PARAMETER DryRun
    Only list what would be copied.

.EXAMPLE
    .\restore-mca.ps1 -DryRun
.EXAMPLE
    .\restore-mca.ps1
#>
[CmdletBinding()]
param(
    [string]$Dimensions = "C:\Users\liqui\AppData\Roaming\horizon-launcher\servers\rs-anarchy\main\dimensions",
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$folders = @("region", "entities", "poi")
$serverProcesses = @("horizon-server", "horizon-client", "horizon-engine")

$logPath = Join-Path (Split-Path -Parent $Dimensions) ("restore-mca-{0:yyyyMMdd-HHmmss}.log" -f (Get-Date))
function Log([string]$message, [string]$color = "Gray") {
    Write-Host $message -ForegroundColor $color
    Add-Content -LiteralPath $logPath -Value $message
}

if (-not (Test-Path -LiteralPath $Dimensions -PathType Container)) {
    throw "Dimensions folder not found: $Dimensions"
}
$world = Split-Path -Parent $Dimensions
Log "Restore original .mca files" "Cyan"
Log "World: $world"
Log ("Mode: " + $(if ($DryRun) { "dry run (nothing copied)" } else { "copy missing files" }))

# 1. The server must be stopped: it holds these files and would write its own copies.
$running = Get-Process -Name $serverProcesses -ErrorAction SilentlyContinue
if ($running) {
    $list = ($running | ForEach-Object { "$($_.ProcessName) (pid $($_.Id))" }) -join ", "
    throw "The server is running ($list). Stop it first, then run this again."
}
$lock = Join-Path $world "session.lock"
if (Test-Path -LiteralPath $lock) {
    try {
        $stream = [System.IO.File]::Open($lock, "Open", "ReadWrite", "None")
        $stream.Close()
    } catch {
        throw "The world is locked ($lock is in use). Stop the server first, then run this again."
    }
}

# 2. Find every dimension that has a backup.
$dimensionFolders = Get-ChildItem -LiteralPath $Dimensions -Directory -Recurse |
    Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName "mca-backup") -PathType Container }

if (-not $dimensionFolders) {
    Log "No mca-backup folders found under $Dimensions. Nothing to restore." "Yellow"
    return
}

# 3. Move the current .mca files aside, then copy the backups in place of them.
#    Nothing is deleted: the current files go to mca-replaced-<timestamp>\<folder>\ in each dimension.
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$moved = 0
$copied = 0
$unchanged = 0
foreach ($dimension in $dimensionFolders) {
    # Windows PowerShell 5.1 (.NET Framework) has no Path.GetRelativePath: cut the prefix by hand.
    $relative = $dimension.FullName.Substring($Dimensions.TrimEnd('\').Length).TrimStart('\')
    Log "Dimension: $relative" "White"
    $archive = Join-Path $dimension.FullName ("mca-replaced-" + $stamp)
    foreach ($folder in $folders) {
        $source = Join-Path $dimension.FullName (Join-Path "mca-backup" $folder)
        if (-not (Test-Path -LiteralPath $source -PathType Container)) { continue }
        $target = Join-Path $dimension.FullName $folder

        # Current .mca files move aside first, unless the backup of the same name is identical.
        if (Test-Path -LiteralPath $target -PathType Container) {
            foreach ($current in Get-ChildItem -LiteralPath $target -Filter "*.mca" -File) {
                $backup = Join-Path $source $current.Name
                if ((Test-Path -LiteralPath $backup) -and
                    ((Get-FileHash -LiteralPath $backup).Hash -eq (Get-FileHash -LiteralPath $current.FullName).Hash)) {
                    Log "  already the backup: $folder\$($current.Name)" "DarkGray"
                    continue
                }
                if ($DryRun) {
                    Log "  would move aside: $folder\$($current.Name) -> mca-replaced-$stamp\$folder\" "Yellow"
                } else {
                    $archiveFolder = Join-Path $archive $folder
                    New-Item -ItemType Directory -Path $archiveFolder -Force | Out-Null
                    Move-Item -LiteralPath $current.FullName -Destination (Join-Path $archiveFolder $current.Name)
                    Log "  moved aside: $folder\$($current.Name)" "DarkYellow"
                }
                $moved++
            }
        }

        # Then the backups go in.
        foreach ($file in Get-ChildItem -LiteralPath $source -Filter "*.mca" -File) {
            $destination = Join-Path $target $file.Name
            if ((Test-Path -LiteralPath $destination) -and
                ((Get-FileHash -LiteralPath $destination).Hash -eq (Get-FileHash -LiteralPath $file.FullName).Hash)) {
                $unchanged++
                continue
            }
            if ($DryRun) {
                Log "  would copy: mca-backup\$folder\$($file.Name) -> $folder\" "Yellow"
            } else {
                New-Item -ItemType Directory -Path $target -Force | Out-Null
                Copy-Item -LiteralPath $file.FullName -Destination $destination
                Log "  restored: $folder\$($file.Name)" "Green"
            }
            $copied++
        }
    }
}

$verb = if ($DryRun) { "would be" } else { "were" }
Log ""
Log "$moved current .mca file(s) $verb moved to mca-replaced-$stamp folders." "Cyan"
Log "$copied backed-up .mca file(s) $verb restored, $unchanged already identical." "Cyan"
Log "Log: $logPath" "DarkGray"
