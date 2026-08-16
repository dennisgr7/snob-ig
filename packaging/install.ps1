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
    #
    # Read and written through the registry rather than through
    # [Environment]::GetEnvironmentVariable, which expands %VAR% references
    # before handing the value over. Writing that back turns a PATH holding
    # %USERPROFILE%\.cargo\bin into the literal expanded path, silently and
    # permanently, which breaks a roaming profile or a relocated home directory
    # for every tool that was relying on it.
    $onPath = $false
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        $userPath = $key.GetValue(
            'Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        # An account that has never had a user PATH has no value to read, and
        # calling a method on the $null that comes back is a terminating error
        # under $ErrorActionPreference = 'Stop' -- after the binary has already
        # been copied, so the install fails having half worked.
        if ($null -eq $userPath) { $userPath = '' }

        # Split on the separator and compare whole entries. `-like` treats [ ]
        # as a character class, so a bracket in the path made the test
        # meaningless; and a substring match called it present when PATH merely
        # contained a longer path starting with this one.
        $entries = $userPath.Split(';') | Where-Object { $_ -ne '' }
        if ($entries -contains $installDir) {
            $onPath = $true
        }
        else {
            $updated = (@($entries) + $installDir) -join ';'
            # ExpandString, so any %VAR% left in the value keeps expanding.
            $key.SetValue('Path', $updated, [Microsoft.Win32.RegistryValueKind]::ExpandString)

            # Writing the value is not enough. Environment blocks are inherited
            # at process start, so a "new terminal" is spawned by the explorer.exe
            # that is already running with the old block and gets the old PATH.
            # Without this broadcast the message below is false until the user
            # logs out, which is exactly what it says is unnecessary.
            #
            # Best-effort on purpose: $ErrorActionPreference is 'Stop', Add-Type
            # compiles at run time and can legitimately fail, and by this point
            # the binary is already installed. A failed broadcast costs a stale
            # terminal; a failed install costs the install.
            $broadcast = $false
            try {
                if (-not ('SnobEnv' -as [type])) {
                    Add-Type -Namespace '' -Name 'SnobEnv' -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(
    IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam,
    uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
                }
                $HWND_BROADCAST = [IntPtr]0xffff
                $WM_SETTINGCHANGE = 0x1a
                $SMTO_ABORTIFHUNG = 0x2
                $result = [UIntPtr]::Zero
                [void][SnobEnv]::SendMessageTimeout(
                    $HWND_BROADCAST, $WM_SETTINGCHANGE, [UIntPtr]::Zero, 'Environment',
                    $SMTO_ABORTIFHUNG, 5000, [ref]$result)
                $broadcast = $true
            }
            catch {
                $broadcast = $false
            }

            if ($broadcast) {
                Write-Host "Added $installDir to your PATH. Open a new terminal for it to take effect."
            }
            else {
                Write-Host "Added $installDir to your PATH. Sign out and back in for it to take effect."
            }
        }
    }
    finally {
        if ($key) { $key.Dispose() }
    }

    # The commands below have to be ones the user can actually type. Until the
    # new PATH reaches a fresh terminal, a bare `snob` does not resolve in this
    # one -- and telling somebody to run a command that fails is how the
    # `snob purge` reminder gets skipped, which leaves a live session cookie on
    # a machine whose owner has just uninstalled the tool.
    $snob = if ($onPath) { 'snob' } else { "`"$exe`"" }

    Write-Host ""
    Write-Host "Start with: $snob login"
    Write-Host "Before uninstalling, run `"$snob purge`": the session and the database"
    Write-Host "live outside this directory and deleting the binary will not reach them."
}
finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
