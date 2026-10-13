# Build the release exe and sign it, so the in-app updater accepts it.
#
#   .\release.ps1
#
# Upload BOTH files to the GitHub release (tag it v<version from Cargo.toml>):
#   target\release\kimrashi.exe
#   target\release\kimrashi.exe.sig
#
# Signing uses the private key in %USERPROFILE%\.kimrashi\signing.key (made once with
# `kimrashi-sign keygen`). Keep that file out of the repository and back it up: without
# it no further update can be signed for the public key built into the program.
# If scripts are blocked: powershell -ExecutionPolicy Bypass -File release.ps1

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# kimrashi-sign signs with its own crate version, and it was just built with the exe.
& .\target\release\kimrashi-sign.exe sign .\target\release\kimrashi.exe
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ''
Write-Host 'Upload these two files to the release:'
Get-Item .\target\release\kimrashi.exe, .\target\release\kimrashi.exe.sig | Format-Table Name, Length -AutoSize
