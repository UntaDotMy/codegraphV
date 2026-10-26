# Install codegraphV (the Rust rewrite of codegraph) from a GitHub Release (Windows x64).
#
#   irm https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.ps1 | iex
#
# Options (environment variables, so the one-liner above can use them too):
#   CODEGRAPHV_VERSION      release tag to install, for example v0.0.2 (default: latest)
#   CODEGRAPHV_INSTALL_DIR  where codegraphV.exe goes (default: %LOCALAPPDATA%\codegraphV\bin)
# The download is checked against the release's SHA256SUMS, and the temporary files are
# deleted afterwards. The install folder is added to your user PATH.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$repo = 'UntaDotMy/codegraphV'

# "latest" comes from the releases/latest web redirect first: the unauthenticated API is
# limited to 60 requests an hour per IP.
function Get-LatestTag {
    try {
        $r = Invoke-WebRequest "https://github.com/$repo/releases/latest" -Method Head -UseBasicParsing
        $uri = if ($r.BaseResponse.ResponseUri) { $r.BaseResponse.ResponseUri } else { $r.BaseResponse.RequestMessage.RequestUri }
        if ("$uri" -match '/releases/tag/([^/]+)$') { return $Matches[1] }
    } catch {}
    (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'codegraphV-install' }).tag_name
}
# `codegraph upgrade` of codegraph v0.0.1 (before the rename) runs this script with the old
# variable names. It gets codegraphV in codegraphV's own folder, then a migrate of its setup.
$legacy = $env:CODEGRAPH_INSTALL_DIR -and -not $env:CODEGRAPHV_INSTALL_DIR
$version = $env:CODEGRAPHV_VERSION
if (-not $version -and $legacy) { $version = $env:CODEGRAPH_VERSION }
if (-not $version) { $version = Get-LatestTag }
if (-not $version) { throw 'Could not find the latest release; set CODEGRAPHV_VERSION (for example v0.0.2).' }
if ($version -notlike 'v*') { $version = "v$version" }
$dir = $env:CODEGRAPHV_INSTALL_DIR
if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'codegraphV\bin' }

$name = "codegraphV-$version-windows-x64"
$base = "https://github.com/$repo/releases/download/$version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("codegraphV-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    Write-Host "Downloading codegraphV $version..."
    Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
    $expected = (Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match "\s\*?$name\.zip$" }) -replace '\s.*$', ''
    $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
    if (-not $expected -or $expected.ToLower() -ne $actual) { throw "Checksum mismatch for $name.zip; nothing was installed." }
    Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp

    New-Item -ItemType Directory -Force $dir | Out-Null
    $target = Join-Path $dir 'codegraphV.exe'
    # Copies earlier upgrades moved aside; one still in use by an open session stays.
    Get-ChildItem $dir -Filter 'codegraphV.exe.old*' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    # A running codegraphV (an MCP server) locks its exe; Windows still lets it be renamed.
    $aside = "$target.old"
    if (Test-Path $aside) { $aside = "$target.old-$([DateTime]::UtcNow.Ticks)" }
    if (Test-Path $target) { Move-Item $target $aside -Force }
    Copy-Item "$tmp\$name\codegraphV.exe" $target
    Remove-Item $aside -Force -ErrorAction SilentlyContinue
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($userPath -split ';') -contains $dir)) {
    [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$dir").TrimStart(';'), 'User')
    Write-Host "Added $dir to your user PATH (open a new terminal to use it)."
}
$env:Path = "$dir;$env:Path"
Write-Host "Installed $(& $target --version) to $target"
if ($legacy) {
    Write-Host "codegraph is now called codegraphV. Moving your agents and indexes over (codegraphV migrate)..."
    & $target migrate --yes
    Write-Host "From now on run 'codegraphV' (in a new terminal). The old 'codegraph upgrade' may now report that codegraph.exe does not report $version; that is expected, because codegraphV.exe replaced it."
} elseif (Get-Command codegraph -ErrorAction SilentlyContinue) {
    # The original codegraph (npm @colbymchenry/codegraph, or codegraph v0.0.1 of this fork).
    Write-Host "Found the original codegraph. Next: run 'codegraphV migrate' to move your agents and indexes over."
} else {
    Write-Host "Next: run 'codegraphV install' to connect Claude Code, Cursor, Codex or opencode."
}
