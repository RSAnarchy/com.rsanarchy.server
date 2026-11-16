<#
.SYNOPSIS
    Builds the RS Anarchy server on a Linux box and installs it.

.DESCRIPTION
    Builds horizon-server and horizon-client (the engine that runs the world; the server starts it)
    with the "fast" profile, then copies both into the install folder.

    Steps:
      1. Preflight: Linux, RAM, free disk space.
      2. System packages (apt), unless -SkipPackages.
      3. Rust toolchain (rustup, stable >= 1.95), installed if missing.
      4. cargo build of both binaries, using fewer parallel jobs on small machines.
      5. Stops a running server gracefully (SIGTERM, then waits), copies the binaries,
         keeps the previous ones as *.previous, and checks the copies by hash.

    Run it from the repository (or anywhere: -Repo points at it).

.PARAMETER Repo
    The repository root. Defaults to the parent of this script's folder.

.PARAMETER Install
    Where the binaries are copied. The server looks for the engine beside itself.

.PARAMETER SkipPackages
    Don't run apt (use when the build packages are already installed).

.EXAMPLE
    pwsh ./scripts/build-linux.ps1
.EXAMPLE
    pwsh ./scripts/build-linux.ps1 -Install /srv/rsanarchy/runtime -SkipPackages
#>
#requires -Version 7
[CmdletBinding()]
param(
    [string]$Repo = (Split-Path -Parent $PSScriptRoot),
    [string]$Install = (Join-Path $HOME "rsanarchy/server-runtime"),
    [switch]$SkipPackages
)

$ErrorActionPreference = "Stop"
$MinRustMajor = 1
$MinRustMinor = 95
$Binaries = @("horizon-server", "horizon-client")
$LogFile = Join-Path $Repo ("build-linux-{0:yyyyMMdd-HHmmss}.log" -f (Get-Date))

function Step([string]$text) { Write-Host "==> $text" -ForegroundColor Cyan }
function Info([string]$text) { Write-Host "    $text" -ForegroundColor Gray }
function Warn([string]$text) { Write-Host "    WARNING: $text" -ForegroundColor Yellow }
function Fail([string]$text) { throw $text }

# 1. Preflight -----------------------------------------------------------------
Step "Preflight"
if (-not $IsLinux) { Fail "This script is for Linux. On Windows, use _build.and.deploy.ps1." }
if (-not (Test-Path -LiteralPath (Join-Path $Repo "Cargo.toml"))) { Fail "No Cargo.toml in $Repo. Pass -Repo." }

$memKiB = [int64]((Get-Content /proc/meminfo | Where-Object { $_ -match '^MemTotal:' }) -replace '\D', '')
$ramGiB = [math]::Round($memKiB / 1MB, 1)
$cores = [Environment]::ProcessorCount
$freeGiB = [math]::Round(([int64](& df -Pk -- $Repo | Select-Object -Last 1).Split(' ', [StringSplitOptions]::RemoveEmptyEntries)[3]) / 1MB, 1)
Info "RAM $ramGiB GiB, $cores CPU threads, $freeGiB GiB free at the repo"
if ($ramGiB -lt 8) { Warn "Under 8 GiB RAM: the build may run out of memory. Add swap or build on a bigger machine." }
if ($freeGiB -lt 20) { Fail "Need about 20 GiB free for the build; $freeGiB GiB available." }

# Fewer parallel jobs on small machines: each rustc job can take a lot of memory.
$jobs = [math]::Max(1, [math]::Min($cores, [math]::Floor($ramGiB / 2)))
Info "cargo jobs: $jobs"

# 2. System packages -----------------------------------------------------------
if (-not $SkipPackages) {
    Step "System packages"
    $isRoot = (& id -u) -eq "0"
    function Apt { if ($isRoot) { & apt-get @args } else { & sudo apt-get @args } }
    $packages = @(
        "build-essential", "pkg-config", "git", "curl", "ca-certificates",
        "libasound2-dev", "libudev-dev", "libxkbcommon-dev", "libwayland-dev",
        "libx11-dev", "libxi-dev", "libxrandr-dev", "libxcursor-dev", "libxinerama-dev", "libvulkan-dev"
    )
    Apt update -q
    if ($LASTEXITCODE -ne 0) { Fail "apt-get update failed." }
    $env:DEBIAN_FRONTEND = "noninteractive"
    Apt install -y -q @packages
    if ($LASTEXITCODE -ne 0) { Fail "apt-get install failed." }
} else {
    Info "Skipping apt packages (-SkipPackages)"
}

# 3. Rust ----------------------------------------------------------------------
Step "Rust toolchain"
$cargoBin = Join-Path $HOME ".cargo/bin"
if (Test-Path -LiteralPath $cargoBin) { $env:PATH = "$cargoBin$([IO.Path]::PathSeparator)$env:PATH" }
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    Info "Installing rustup (stable, minimal profile)"
    & sh -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable'
    if ($LASTEXITCODE -ne 0) { Fail "rustup install failed." }
    $env:PATH = "$cargoBin$([IO.Path]::PathSeparator)$env:PATH"
}
$rustc = (& rustc --version) -join ""
if ($rustc -notmatch 'rustc (\d+)\.(\d+)') { Fail "Could not read the Rust version from: $rustc" }
$major = [int]$Matches[1]; $minor = [int]$Matches[2]
if ($major -lt $MinRustMajor -or ($major -eq $MinRustMajor -and $minor -lt $MinRustMinor)) {
    Info "$rustc is older than $MinRustMajor.$MinRustMinor; updating stable"
    & rustup toolchain install stable --profile minimal
    & rustup default stable
    $rustc = (& rustc --version) -join ""
}
Info $rustc

# 4. Build ---------------------------------------------------------------------
Step "Building $($Binaries -join ' and ') (profile fast, log: $LogFile)"
$env:CARGO_BUILD_JOBS = "$jobs"
Push-Location $Repo
try {
    $args = @("build", "--locked", "--profile", "fast")
    foreach ($bin in $Binaries) { $args += @("-p", $bin) }
    & cargo @args *>&1 | Tee-Object -FilePath $LogFile | Where-Object { $_ -match '^(error|warning: unused|\s+Finished|\s+Compiling (horizon-(server|client)))' } | ForEach-Object { Info $_ }
    if ($LASTEXITCODE -ne 0) {
        $tail = (Get-Content -LiteralPath $LogFile -Tail 30) -join "`n"
        Fail "cargo build failed (exit $LASTEXITCODE). Last lines of ${LogFile}:`n$tail"
    }
} finally {
    Pop-Location
}

$built = @{}
foreach ($bin in $Binaries) {
    $path = Join-Path $Repo "target/fast/$bin"
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { Fail "Build output missing: $path" }
    $built[$bin] = $path
    Info ("{0}: {1}" -f $bin, (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.Substring(0, 16))
}

# 5. Stop, install, verify -----------------------------------------------------
Step "Installing into $Install"
$running = Get-Process -Name $Binaries -ErrorAction SilentlyContinue
if ($running) {
    Info ("Stopping: " + (($running | ForEach-Object { "$($_.ProcessName) (pid $($_.Id))" }) -join ", "))
    $running | Stop-Process
    $deadline = (Get-Date).AddSeconds(90)
    while ((Get-Process -Name $Binaries -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2
    }
    if (Get-Process -Name $Binaries -ErrorAction SilentlyContinue) {
        Fail "The server did not stop within 90 seconds. It was not force-killed, so its files are safe. Stop it and run again."
    }
}

New-Item -ItemType Directory -Path $Install -Force | Out-Null
foreach ($bin in $Binaries) {
    $destination = Join-Path $Install $bin
    if (Test-Path -LiteralPath $destination) {
        Copy-Item -LiteralPath $destination -Destination "$destination.previous" -Force
    }
    Copy-Item -LiteralPath $built[$bin] -Destination $destination -Force
    & chmod +x -- $destination
    if ((Get-FileHash -LiteralPath $built[$bin]).Hash -ne (Get-FileHash -LiteralPath $destination).Hash) {
        Fail "$bin does not match its build after the copy."
    }
    Info "installed $bin (hash checked)"
}

Step "Done"
Write-Host "Start the server from its world folder with:" -ForegroundColor Green
Write-Host "    cd <server folder>; $(Join-Path $Install 'horizon-server') run" -ForegroundColor Green
Write-Host "The engine (horizon-client) must sit beside horizon-server; both were installed to $Install." -ForegroundColor DarkGray
