<#
.SYNOPSIS
    Builds the Linux server binaries from Windows, inside WSL.

.DESCRIPTION
    1. Copies the repository into the WSL distro's own Linux filesystem (building on /mnt/c is
       much slower), leaving the WSL build folder's target/ in place between builds.
    2. Runs scripts/build-linux.sh there (packages, Rust, cargo build of horizon-server and
       horizon-client, profile fast).
    3. Copies the two binaries back to a folder on Windows and checks their hashes.

    Works with Windows PowerShell 5.1 and PowerShell 7.

.PARAMETER Distro
    The WSL distribution to build in. Default: WSL's default distribution.

.PARAMETER Repo
    The repository folder on Windows. Default: the parent of this script's folder.

.PARAMETER Out
    Where the binaries are copied on Windows. Default: <Repo>\dist\linux.

.PARAMETER Jobs
    Parallel cargo jobs inside WSL. Default: the build script's choice (CPU count, capped by RAM).

.PARAMETER SkipPackages
    Don't install system packages in WSL (use once they are installed).

.PARAMETER Clean
    Remove the WSL build folder's target/ first (a full rebuild).

.EXAMPLE
    .\scripts\build-wsl.ps1
.EXAMPLE
    .\scripts\build-wsl.ps1 -Distro Ubuntu-24.04 -Jobs 6
#>
[CmdletBinding()]
param(
    [string]$Distro = "",
    [string]$Repo = "",
    [string]$Out = "",
    [int]$Jobs = 0,
    [switch]$SkipPackages,
    [switch]$Clean
)

$ErrorActionPreference = "Stop"

function Step([string]$text) { Write-Host "==> $text" -ForegroundColor Cyan }
function Info([string]$text) { Write-Host "    $text" -ForegroundColor Gray }
function Fail([string]$text) { throw $text }

# Runs a command in the distro as its default user (or as root when -AsRoot). Plain sh, so it
# works on distros without bash; commands that need bash say so explicitly.
# Output is returned as text; a failing command throws.
function Get-WslArgs([switch]$AsRoot) {
    $wslArgs = @()
    if ($Distro) { $wslArgs += @("-d", $Distro) }
    if ($AsRoot) { $wslArgs += @("-u", "root") }
    return $wslArgs
}

# Windows strips double quotes from arguments given to wsl.exe, which breaks shell commands with
# quotes in them. So the command travels base64-encoded (only letters, digits, + / = and |) and
# WSL decodes and runs it.
function Get-WslShellArgs([string]$Script) {
    $encoded = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($Script))
    return @("sh", "-c", "echo $encoded | base64 -d | sh")
}

function Invoke-Wsl {
    param([string]$Command, [switch]$AsRoot)
    # Windows PowerShell 5.1 turns a native command's stderr into errors; read the exit code instead.
    # stdout is the result; stderr (WSL's own notices included) goes to a file, shown only on failure.
    $errFile = [System.IO.Path]::GetTempFileName()
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & wsl.exe (Get-WslArgs -AsRoot:$AsRoot) -- (Get-WslShellArgs $Command) 2>$errFile
        $code = $LASTEXITCODE
        $err = (Get-Content -LiteralPath $errFile -Raw -ErrorAction SilentlyContinue)
    } finally {
        $ErrorActionPreference = $previous
        Remove-Item -LiteralPath $errFile -Force -ErrorAction SilentlyContinue
    }
    if ($code -ne 0) {
        $detail = (($output | Out-String).Trim() + "`n" + ([string]$err).Trim()).Trim()
        Fail ("WSL command failed (exit $code): $Command`n" + $detail)
    }
    return (($output | Out-String).Trim())
}

# 0. Preflight -----------------------------------------------------------------
Step "Preflight"
if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
    Fail "wsl.exe not found. Install WSL first (wsl --install)."
}
if (-not $Repo) {
    $candidates = @()
    if ($PSScriptRoot) { $candidates += (Split-Path -Parent $PSScriptRoot) }
    $candidates += (Get-Location).Path
    $Repo = $candidates | Where-Object { $_ -and (Test-Path -LiteralPath (Join-Path $_ "Cargo.toml")) } | Select-Object -First 1
    if (-not $Repo) { Fail "Could not find the repository (no Cargo.toml). Run from the repository folder, or pass -Repo." }
}
$Repo = (Resolve-Path -LiteralPath $Repo).Path
if (-not $Out) { $Out = Join-Path $Repo "dist\linux" }
New-Item -ItemType Directory -Path $Out -Force | Out-Null
$Out = (Resolve-Path -LiteralPath $Out).Path
Info "repo: $Repo"
Info "output: $Out"

$system = Invoke-Wsl "uname -s"
if ($system -ne "Linux") { Fail "WSL did not answer like Linux (got '$system'). Check the distro with: wsl -l -v" }

# The build script needs bash, and the packages step needs apt.
$hasBash = Invoke-Wsl "command -v bash >/dev/null 2>&1 && echo yes || echo no"
if ($hasBash -ne "yes") {
    Fail ("This WSL distro has no bash, and the build needs it. Install Ubuntu and build there:`n" +
          "    wsl --install -d Ubuntu-24.04`n" +
          "    then: .\scripts\build-wsl.ps1 -Distro Ubuntu-24.04`n" +
          "Your distros: run 'wsl -l -v'.")
}
$distroId = Invoke-Wsl '. /etc/os-release 2>/dev/null; echo "${ID:-unknown} ${VERSION_ID:-}"'
Info "distro: $distroId"
if ($distroId -notmatch '^(ubuntu|debian)') {
    Write-Host "    WARNING: the build packages are installed with apt, and binaries should match the Linux box. Ubuntu is the tested choice (wsl --install -d Ubuntu-24.04)." -ForegroundColor Yellow
}
$wslRepo = Invoke-Wsl "wslpath -a '$($Repo.Replace("'", "'\''"))'"
$wslOut = Invoke-Wsl "wslpath -a '$($Out.Replace("'", "'\''"))'"
$wslHome = Invoke-Wsl 'printf %s "$HOME"'
$build = "$wslHome/horizon-build"
Info "WSL build folder: $build"

# Memory: WSL caps its VM at half the host RAM unless .wslconfig says otherwise.
$wslMem = Invoke-Wsl "awk '/MemTotal/ { printf ""%.1f"", `$2 / 1048576 }' /proc/meminfo"
Info "WSL memory: $wslMem GiB"
if ([double]$wslMem -lt 8) {
    Write-Host "    WARNING: WSL sees under 8 GiB. Raise it in %UserProfile%\.wslconfig ([wsl2] memory=12GB), then run: wsl --shutdown" -ForegroundColor Yellow
}

# 1. Packages (as root, so no password prompt) --------------------------------
if (-not $SkipPackages) {
    Step "System packages in WSL"
    $packages = "build-essential pkg-config git curl ca-certificates rsync libasound2-dev libudev-dev libxkbcommon-dev libwayland-dev libx11-dev libxi-dev libxrandr-dev libxcursor-dev libxinerama-dev libvulkan-dev"
    Invoke-Wsl "export DEBIAN_FRONTEND=noninteractive; apt-get update -q && apt-get install -y -q $packages" -AsRoot | Out-Null
    Info "installed"
}

# 2. Sync the repo into WSL ----------------------------------------------------
Step "Syncing the repository into WSL"
if ($Clean) {
    Invoke-Wsl "rm -rf '$build/target'" | Out-Null
    Info "cleaned target/"
}
# target/ and .git/ stay in WSL: excluded from the copy and protected from --delete.
Invoke-Wsl "mkdir -p '$build' && rsync -a --delete --exclude target/ --exclude .git/ --exclude 'build-linux-*.log' --exclude dist/ '$wslRepo/' '$build/'" | Out-Null
# Files checked out on Windows have CRLF line endings; bash needs LF.
Invoke-Wsl "sed -i 's/\r`$//' '$build/scripts/build-linux.sh'" | Out-Null
Info "synced"

# 3. Build ---------------------------------------------------------------------
Step "Building in WSL (this can take a long time the first time)"
# The packages were installed above (as root), so the build itself never needs them.
$jobArg = if ($Jobs -gt 0) { " --jobs $Jobs" } else { "" }
$previous = $ErrorActionPreference
$ErrorActionPreference = "Continue"
try {
    $buildScript = "cd '$build' && bash scripts/build-linux.sh --skip-packages$jobArg"
    $buildOutput = & wsl.exe (Get-WslArgs) -- (Get-WslShellArgs $buildScript) 2>&1
    $buildCode = $LASTEXITCODE
} finally {
    $ErrorActionPreference = $previous
}
$buildOutput | ForEach-Object { Write-Host "    $_" }
if ($buildCode -ne 0) { Fail "The WSL build failed (see the output above and the build-linux-*.log in the WSL build folder)." }

# 4. Copy out and verify -------------------------------------------------------
Step "Copying binaries to $Out"
$binaries = @("horizon-server", "horizon-client")
foreach ($bin in $binaries) {
    Invoke-Wsl "cp '$build/target/fast/$bin' '$wslOut/$bin' && chmod +x '$wslOut/$bin'" | Out-Null
    $windowsCopy = Join-Path $Out $bin
    $builtHash = (Invoke-Wsl "sha256sum '$wslOut/$bin' | cut -d' ' -f1").Trim()
    $copyHash = (Get-FileHash -LiteralPath $windowsCopy -Algorithm SHA256).Hash.ToLower()
    if ($builtHash -ne $copyHash) { Fail "$bin does not match after the copy (WSL $builtHash, Windows $copyHash)." }
    Info "$bin  sha256 $copyHash"
}
$sums = $binaries | ForEach-Object { "{0}  {1}" -f ((Get-FileHash -LiteralPath (Join-Path $Out $_) -Algorithm SHA256).Hash.ToLower()), $_ }
Set-Content -LiteralPath (Join-Path $Out "SHA256SUMS") -Value $sums -Encoding ASCII

Step "Done"
Write-Host "Linux binaries are in: $Out" -ForegroundColor Green
Write-Host "Copy them to the Linux box, then: chmod +x horizon-server horizon-client" -ForegroundColor DarkGray
