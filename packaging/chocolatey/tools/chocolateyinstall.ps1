$ErrorActionPreference = 'Stop'

# Portable install: download the Windows release zip and unpack it into the
# package's tools directory. Chocolatey auto-shims the .exe files it finds there
# (gila.exe, gila-headless.exe) onto the PATH. @VERSION@ / @SHA256_WINDOWS_X64@
# are substituted by the release workflow at pack time.
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

$packageArgs = @{
  packageName    = 'gila'
  unzipLocation  = $toolsDir
  url64bit       = 'https://github.com/Gilamonster-Foundation/gilamonster-agent/releases/download/v@VERSION@/gilamonster-agent-v@VERSION@-windows-x86_64.zip'
  checksum64     = '@SHA256_WINDOWS_X64@'
  checksumType64 = 'sha256'
}

Install-ChocolateyZipPackage @packageArgs
