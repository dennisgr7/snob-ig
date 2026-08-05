<#
.SYNOPSIS
    Installs snob on Windows.

.DESCRIPTION
    irm https://raw.githubusercontent.com/dennisgr7/snob-ig/main/packaging/install.ps1 | iex

    Reads two optional environment variables:

        SNOB_VERSION      a version to install instead of the latest
        SNOB_INSTALL_DIR  where to put it; default %LOCALAPPDATA%\Programs\snob
#>

$ErrorActionPreference = 'Stop'

$repo = 'dennisgr7/snob-ig'
$installDir = if ($env:SNOB_INSTALL_DIR) { $env:SNOB_INSTALL_DIR }
              else { Join-Path $env:LOCALAPPDATA 'Programs\snob' }

# ARM64 Windows reports AMD64 to a 32-bit or emulated process, so the
# environment variable alone is not enough to tell the two apart.
$arch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    'Arm64' { 'aarch64-pc-windows-msvc' }
    'X64'   { 'x86_64-pc-windows-msvc' }
    default { throw "snob has no build for $_" }
}

$version = $env:SNOB_VERSION
if (-not $version) {
    $latest = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
    $version = $latest.tag_name -replace '^v', ''
}

$name = "snob-v$version-$arch"
$base = "https://github.com/$repo/releases/download/v$version"
$work = Join-Path ([System.IO.Path]::GetTempPath()) "snob-install-$([System.Guid]::NewGuid())"
New-Item -ItemType Directory -Force $work | Out-Null

try {
    Write-Host "Downloading snob $version for $arch"
    $zip = Join-Path $work "$name.zip"
    Invoke-WebRequest "$base/$name.zip" -OutFile $zip
    $sumsFile = Join-Path $work 'SHA256SUMS'
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile $sumsFile

    # Not optional: this downloads an executable and puts it on the PATH.
    $expected = ((Get-Content $sumsFile | Select-String -SimpleMatch "$name.zip") -split '\s+')[0]
    if (-not $expected) { throw "$name.zip is not listed in SHA256SUMS" }
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "checksum mismatch: expected $expected, got $actual" }

    Expand-Archive $zip -DestinationPath $work -Force
    New-Item -ItemType Directory -Force $installDir | Out-Null
    Copy-Item (Join-Path $work "$name\*") $installDir -Force

    $exe = Join-Path $installDir 'snob.exe'
    Write-Host "Installed $(& $exe --version) to $installDir"

    # The user's PATH, never the machine's: this installs for one account and
    # has no business editing anything the whole system reads.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$installDir*") {
        [Environment]::SetEnvironmentVariable(
            'Path', ($userPath.TrimEnd(';') + ';' + $installDir), 'User')
        Write-Host "Added $installDir to your PATH. Open a new terminal for it to take effect."
    }

    Write-Host ""
    Write-Host "Start with: snob login"
    Write-Host "Before uninstalling, run `"snob purge`": the session and the database"
    Write-Host "live outside this directory and deleting the binary will not reach them."
}
finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
