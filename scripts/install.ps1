# One-line install for Windows:
#   irm https://raw.githubusercontent.com/bgauryy/FreeLlama/main/scripts/install.ps1 | iex
# Installs FreeLlama, then checks for Ollama. When Ollama is missing it asks before running Ollama's
# official installer (https://ollama.com/install.ps1). It never downloads a model.
param(
  # A release tag (default: the latest release). A pinned version keeps the verified-binary install.
  [string]$Version = "",
  # npm: @octocodeai/freellama with the MCP server and agent commands; binary: the verified operator binary.
  [ValidateSet("", "npm", "binary")][string]$Method = "",
  [string]$BinDir = "$env:LOCALAPPDATA\FreeLlama\bin",
  [switch]$WithOllama,
  [switch]$NoOllama
)
$ErrorActionPreference = "Stop"

function Install-Npm {
  if (-not (Get-Command npm -ErrorAction SilentlyContinue)) { throw "npm is not on PATH; rerun with -Method binary, or install Node.js 22+" }
  $package = "@octocodeai/freellama"
  if ($Version) { $package = "$package@$($Version.TrimStart('v'))" }
  Write-Output "Installing $package with npm"
  npm install -g $package
  if ($LASTEXITCODE -ne 0) { throw "npm install -g $package failed" }
}

function Install-Binary {
  $architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
  switch ($architecture.ToString()) {
    "X64" { $target = "win32-x64-msvc" }
    "Arm64" { $target = "win32-arm64-msvc" }
    default { throw "Unsupported Windows architecture: $architecture" }
  }
  $asset = "freellama-$target.exe"
  if ($env:FREELLAMA_RELEASE_BASE) { $base = $env:FREELLAMA_RELEASE_BASE }
  elseif ($Version) { $base = "https://github.com/bgauryy/FreeLlama/releases/download/$Version" }
  else { $base = "https://github.com/bgauryy/FreeLlama/releases/latest/download" }
  $label = if ($Version) { $Version } else { "the latest release" }
  Invoke-WebRequest -Uri "$base/$asset" -OutFile (Join-Path $temporary $asset)
  Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile (Join-Path $temporary "SHA256SUMS")
  $line = Get-Content (Join-Path $temporary "SHA256SUMS") | Where-Object { $_ -match "  $([regex]::Escape($asset))$" }
  if (-not $line) { throw "$label has no checksum published for $asset" }
  $expected = ($line -split "\s+")[0].ToLowerInvariant()
  $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $temporary $asset)).Hash.ToLowerInvariant()
  if ($actual -ne $expected) { throw "Checksum mismatch for $asset" }
  New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
  Copy-Item -Force (Join-Path $temporary $asset) (Join-Path $BinDir "freellama.exe")
  Write-Output "Installed verified $label to $BinDir\freellama.exe"
  if (-not (($env:Path -split ";") -contains $BinDir)) { Write-Output "Add $BinDir to PATH to run freellama." }
}

# The same places packages/mcp/src/ollama-presence.ts checks.
function Test-Ollama {
  if (Get-Command ollama -ErrorAction SilentlyContinue) { return $true }
  return [bool]($env:LOCALAPPDATA -and (Test-Path (Join-Path $env:LOCALAPPDATA "Programs\Ollama\ollama.exe")))
}

function Install-OllamaIfMissing {
  $installer = if ($env:FREELLAMA_OLLAMA_INSTALLER) { $env:FREELLAMA_OLLAMA_INSTALLER } else { "https://ollama.com/install.ps1" }
  $command = "irm $installer | iex"
  if (Test-Ollama) { Write-Output "Ollama is installed."; return }
  $answer = [bool]$WithOllama
  if (-not $WithOllama -and -not $NoOllama -and [Environment]::UserInteractive -and -not [Console]::IsInputRedirected) {
    $reply = Read-Host "Ollama is not installed. Run its official installer now ($command)? [y/N]"
    $answer = $reply -match '^(y|yes)$'
  }
  if ($answer) {
    Write-Output "Installing Ollama: $command"
    $file = Join-Path $temporary "ollama-install.ps1"
    Invoke-WebRequest -Uri $installer -OutFile $file
    Invoke-Expression (Get-Content -Raw $file)
  } else {
    Write-Output "Ollama is not installed. Install it with: $command (or https://ollama.com/download)"
  }
}

if (-not $Method) {
  $Method = if (-not $Version -and (Get-Command npm -ErrorAction SilentlyContinue)) { "npm" } else { "binary" }
}
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("freellama-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
  if ($Method -eq "npm") { Install-Npm } else { Install-Binary }
  Install-OllamaIfMissing
  Write-Output "Next: freellama init (checks Ollama, serve and installed models; it never downloads a model)."
  if ($Method -eq "npm") { Write-Output "MCP: claude mcp add freellama -- npx -y @octocodeai/freellama-mcp-server" }
} finally {
  Remove-Item -Recurse -Force $temporary -ErrorAction SilentlyContinue
}
