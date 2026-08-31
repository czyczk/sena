# Windows view: build Windows targets with the locally installed MSVC.
param(
  [ValidateSet("auto","2022","2026")] [string]$VS = "auto",
  [switch]$Debug
)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path
Set-Location $root
$py = (Get-Command python -ErrorAction SilentlyContinue)
if (-not $py) { $py = (Get-Command py -ErrorAction SilentlyContinue) }
if (-not $py) { throw "python not found; install Python 3.10+ first" }
& $py.Source plugins/foobar2000/scripts/build.py windows --vs $VS
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $py.Source plugins/foobar2000/scripts/build.py package
