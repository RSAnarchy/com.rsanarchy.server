# Builds the RSAnarchy server, copies it into the server folder, checks the copies by hash, and
# optionally starts the server.
#
# Layout (under $Root, default C:\Users\liqui\Desktop\RSAnarchy):
#   rsanarchy.server\  the binaries: horizon-server.exe, and horizon-client.exe, the engine the
#                      server runs to load and run the world. The script only writes here.
#   rsanarchy\         the server itself: world, server.toml, server.properties, logs. The
#                      script never copies into or deletes from it.
#
# Usage:
#   .\_build.and.deploy.ps1                 build, copy, verify
#   .\_build.and.deploy.ps1 -Start          also start the server, from rsanarchy\
#   .\_build.and.deploy.ps1 -StopRunning    stop a running server first (otherwise the script stops)
#   .\_build.and.deploy.ps1 -SkipBuild      copy what's already in target\fast (for testing)

param(
    [string]$Repo = "C:\Users\liqui\Documents\GitHub\horizon",
    [string]$Root = "C:\Users\liqui\Desktop\RSAnarchy",
    [switch]$Start,
    [switch]$StopRunning,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$binaries = @("horizon-server.exe", "horizon-client.exe")
$processes = @("horizon-server", "horizon-client")
$target = Join-Path $Repo "target\fast"
$serverBin = Join-Path $Root "rsanarchy.server"
$world = Join-Path $Root "rsanarchy"

function Step([string]$text) { Write-Host "==> $text" -ForegroundColor Cyan }

if (-not (Test-Path -LiteralPath $Repo)) { throw "repo not found: $Repo" }
Set-Location -LiteralPath $Repo

# 1. Build. The engine (horizon-client) needs the generated mod-API inputs at the repo root.
if (-not $SkipBuild) {
    Step "Checking the client's generated inputs"
    $missing = @()
    if (-not (Test-Path -LiteralPath "native-hooks.json")) { $missing += "native-hooks.json" }
    if (-not (Test-Path -LiteralPath "tools\native-hook-fragments")) { $missing += "tools\native-hook-fragments" }
    if ($missing) { throw "missing for the client build: $($missing -join ', '). Nothing was built or copied." }

    Step "Building horizon-server and horizon-client"
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    cargo build --locked --profile fast -p horizon-server -p horizon-client 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
    if ($code -ne 0) { throw "cargo build failed (exit $code); nothing was copied." }
}
foreach ($bin in $binaries) {
    if (-not (Test-Path -LiteralPath (Join-Path $target $bin))) { throw "missing build output: $(Join-Path $target $bin)" }
}

# 2. Running server: its binaries are locked while it runs.
Step "Checking for a running server"
$running = Get-Process -Name $processes -ErrorAction SilentlyContinue
if ($running) {
    $list = ($running | ForEach-Object { "$($_.ProcessName) (pid $($_.Id))" }) -join ", "
    if (-not $StopRunning) {
        throw "The server is running ($list). Stop it first (type 'stop' in its console), or run again with -StopRunning. Nothing was copied."
    }
    $running | ForEach-Object { Write-Host "    stopping $($_.ProcessName) (pid $($_.Id))" }
    $running | Stop-Process -Force
    Start-Sleep -Seconds 1
}

# 3. Copy into rsanarchy.server, keeping the previous copies as .previous.
Step "Copying to $serverBin"
New-Item -ItemType Directory -Path $serverBin -Force | Out-Null
foreach ($bin in $binaries) {
    $destination = Join-Path $serverBin $bin
    if (Test-Path -LiteralPath $destination) {
        Copy-Item -LiteralPath $destination -Destination "$destination.previous" -Force
    }
    Copy-Item -LiteralPath (Join-Path $target $bin) -Destination $destination -Force
}

# 4. Verify by hash.
Step "Verifying"
foreach ($bin in $binaries) {
    $built = (Get-FileHash -LiteralPath (Join-Path $target $bin)).Hash
    $copied = (Get-FileHash -LiteralPath (Join-Path $serverBin $bin)).Hash
    if ($built -ne $copied) { throw "$bin did not match after the copy." }
    Write-Host "    OK   $bin  $($built.Substring(0, 12))" -ForegroundColor Green
}

# 5. The world folder: created if missing, never written by the script.
New-Item -ItemType Directory -Path $world -Force | Out-Null
if (-not (Test-Path -LiteralPath (Join-Path $world "server.toml"))) {
    Write-Host "    note: no server.toml in $world yet; the server will create its defaults." -ForegroundColor Yellow
}

# 6. Start, or say how.
$exe = Join-Path $serverBin "horizon-server.exe"
if ($Start) {
    Step "Starting the server from $world"
    Set-Location -LiteralPath $world
    & $exe run
} else {
    Step "Done"
    Write-Host "Start the server with:" -ForegroundColor Green
    Write-Host "    Set-Location `"$world`"; & `"$exe`" run"
}
