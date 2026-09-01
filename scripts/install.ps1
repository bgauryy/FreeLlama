param(
  [Parameter(Mandatory = $true)][string]$Version,
  [string]$BinDir = "$env:LOCALAPPDATA\FreeLlama\bin"
)
$ErrorActionPreference = "Stop"
if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne "X64") {
  throw "The published Windows package currently supports x64 only."
}
$asset = "freellama-win32-x64.exe"
$base = "https://github.com/bgauryy/FreeLlama/releases/download/$Version"
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("freellama-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
  Invoke-WebRequest -Uri "$base/$asset" -OutFile (Join-Path $temporary $asset)
  Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile (Join-Path $temporary "SHA256SUMS")
  $line = Get-Content (Join-Path $temporary "SHA256SUMS") | Where-Object { $_ -match "  $([regex]::Escape($asset))$" }
  if (-not $line) { throw "No checksum published for $asset" }
  $expected = ($line -split "\s+")[0].ToLowerInvariant()
  $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $temporary $asset)).Hash.ToLowerInvariant()
  if ($actual -ne $expected) { throw "Checksum mismatch for $asset" }
  New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
  Copy-Item -Force (Join-Path $temporary $asset) (Join-Path $BinDir "freellama.exe")
  Write-Output "Installed verified $Version to $BinDir\freellama.exe"
} finally {
  Remove-Item -Recurse -Force $temporary -ErrorAction SilentlyContinue
}
