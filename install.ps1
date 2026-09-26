# Install the latest `helixrouter` release on Windows.
#
#   irm https://raw.githubusercontent.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/main/install.ps1 | iex
#
# Downloads the Windows zip from GitHub Releases, checks it against
# SHA256SUMS.txt, puts the binaries in %LOCALAPPDATA%\Programs\helixrouter and
# adds that folder to your user PATH. Pin a version with $env:VERSION = "v1.2.1".
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'Mattbusel/HelixRouter-adaptive-async-compute-router-'
$Name = 'helixrouter'
$Target = 'x86_64-pc-windows-msvc'
$Dest = Join-Path $env:LOCALAPPDATA "Programs\$Name"

if ($env:VERSION) {
    $Tag = $env:VERSION
} else {
    $Tag = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'install.ps1' }).tag_name
}
if (-not $Tag) { throw "Could not find the latest release of $Repo." }

$Asset = "$Name-$Tag-$Target.zip"
$Base = "https://github.com/$Repo/releases/download/$Tag"
$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("helixrouter-install-" + [guid]::NewGuid())
New-Item -ItemType Directory $Tmp | Out-Null
try {
    Write-Host "Downloading $Asset"
    Invoke-WebRequest "$Base/$Asset" -OutFile (Join-Path $Tmp $Asset) -UseBasicParsing
    Invoke-WebRequest "$Base/SHA256SUMS.txt" -OutFile (Join-Path $Tmp 'SHA256SUMS.txt') -UseBasicParsing

    $line = Get-Content (Join-Path $Tmp 'SHA256SUMS.txt') | Where-Object { $_ -match ([regex]::Escape($Asset) + '$') } | Select-Object -First 1
    if (-not $line) { throw "$Asset is not listed in SHA256SUMS.txt" }
    $expected = ($line -split '\s+')[0].ToLower()
    $actual = (Get-FileHash (Join-Path $Tmp $Asset) -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "Checksum mismatch for $Asset (expected $expected, got $actual)" }
    Write-Host "Checksum OK"

    Expand-Archive (Join-Path $Tmp $Asset) -DestinationPath $Tmp -Force
    New-Item -ItemType Directory $Dest -Force | Out-Null
    Get-ChildItem (Join-Path $Tmp "$Name-$Tag-$Target") -Filter *.exe | ForEach-Object {
        Copy-Item $_.FullName $Dest -Force
    }
} finally {
    Remove-Item $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($UserPath -split ';') -contains $Dest)) {
    $NewPath = if ($UserPath) { "$UserPath;$Dest" } else { $Dest }
    [Environment]::SetEnvironmentVariable('Path', $NewPath, 'User')
    Write-Host "Added $Dest to your user PATH (open a new terminal to pick it up)."
}
$env:Path = "$env:Path;$Dest"

$Version = & (Join-Path $Dest 'helixrouter.exe') --version
Write-Host "Installed $Version to $Dest"
Write-Host ""
Write-Host "Start it:  helixrouter    then open http://127.0.0.1:8080 (open a new terminal first if PATH was just updated)"
