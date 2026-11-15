# Builds horizon-server, stops the running server process,
# copies the new binary into server-runtime\current, checks the copy by hash,
# and optionally starts the server again.
#
# Usage:
#   .\_build.and.deploy.ps1           # build, copy, verify
#   .\_build.and.deploy.ps1 -Start    # also start the server afterwards

param(
    [switch]$Start
)

$ErrorActionPreference = "Stop"

$repo    = "C:\Users\liqui\Documents\GitHub\horizon"
$target  = Join-Path $repo "target\fast"
$runtime = "C:\Users\liqui\AppData\Roaming\horizon-launcher\server-runtime\current"
$server  = "C:\Users\liqui\AppData\Roaming\horizon-launcher\servers\rs-anarchy"
$exe     = "horizon-server.exe"

# Work from the repo, wherever the script was started from.
if (-not (Test-Path $repo)) { throw "repo not found: $repo" }
Set-Location $repo
Write-Host "Working in $repo" -ForegroundColor DarkGray

# 1. Build server only
Write-Host "==> Building horizon-server" -ForegroundColor Cyan
$ErrorActionPreference = "Continue"
cargo build --profile fast -p horizon-server 2>&1 | ForEach-Object { "$_" }
$ErrorActionPreference = "Stop"
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed (exit $LASTEXITCODE); nothing was copied."
}

# 2. Stop the running server so its exe is not locked
Write-Host "==> Stopping running server" -ForegroundColor Cyan
Get-Process -Name "horizon-server" -ErrorAction SilentlyContinue |
    ForEach-Object {
        Write-Host "    stopping $($_.ProcessName) (pid $($_.Id))"
        Stop-Process -Id $_.Id -Force
    }
Start-Sleep -Milliseconds 500

# 3. Copy
Write-Host "==> Copying to $runtime" -ForegroundColor Cyan
if (-not (Test-Path $runtime)) {
    New-Item -ItemType Directory -Path $runtime | Out-Null
}
$src = Join-Path $target $exe
if (-not (Test-Path $src)) { throw "missing build output: $src" }
Copy-Item $src $runtime -Force
Write-Host "    copied $exe"

# 4. Verify by hash
Write-Host "==> Verifying" -ForegroundColor Cyan
$a = (Get-FileHash (Join-Path $target $exe)).Hash
$b = (Get-FileHash (Join-Path $runtime $exe)).Hash
if ($a -eq $b) {
    Write-Host "    OK   $exe  $($a.Substring(0,12))" -ForegroundColor Green
} else {
    Write-Host "    FAIL $exe  built=$($a.Substring(0,12)) deployed=$($b.Substring(0,12))" -ForegroundColor Red
    throw "Binary did not match after copy."
}

# 5. Start (optional)
if ($Start) {
    Write-Host "==> Starting server" -ForegroundColor Cyan
    Set-Location $server
    & (Join-Path $runtime "horizon-server.exe") run
} else {
    Write-Host "Done. Start the server with:" -ForegroundColor Green
    Write-Host "  Set-Location `"$server`"; & `"$runtime\horizon-server.exe`" run"
}
