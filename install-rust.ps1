# Install the Rust codegraph from a GitHub Release (Windows x64).
#
#   irm https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.ps1 | iex
#
# Options (environment variables, so the one-liner above can use them too):
#   CODEGRAPH_VERSION      release tag to install, for example v0.0.1 (default: latest)
#   CODEGRAPH_INSTALL_DIR  where codegraph.exe goes (default: %LOCALAPPDATA%\codegraph\bin)
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
    (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'codegraph-install' }).tag_name
}
$version = $env:CODEGRAPH_VERSION
if (-not $version) { $version = Get-LatestTag }
if (-not $version) { throw 'Could not find the latest release; set CODEGRAPH_VERSION (for example v0.0.1).' }
if ($version -notlike 'v*') { $version = "v$version" }
$dir = $env:CODEGRAPH_INSTALL_DIR
if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'codegraph\bin' }

$name = "codegraph-$version-windows-x64"
$base = "https://github.com/$repo/releases/download/$version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("codegraph-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    Write-Host "Downloading codegraph $version..."
    Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
    $expected = (Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match "\s\*?$name\.zip$" }) -replace '\s.*$', ''
    $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
    if (-not $expected -or $expected.ToLower() -ne $actual) { throw "Checksum mismatch for $name.zip; nothing was installed." }
    Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp

    New-Item -ItemType Directory -Force $dir | Out-Null
    $target = Join-Path $dir 'codegraph.exe'
    Remove-Item "$target.old" -Force -ErrorAction SilentlyContinue
    # A running codegraph (an MCP server) locks its exe; Windows still lets it be renamed.
    if (Test-Path $target) { Move-Item $target "$target.old" -Force }
    Copy-Item "$tmp\$name\codegraph.exe" $target
    Remove-Item "$target.old" -Force -ErrorAction SilentlyContinue
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
# Another codegraph earlier on PATH (often `npm i -g @colbymchenry/codegraph`) would run instead.
$first = Get-Command codegraph -All -ErrorAction SilentlyContinue | Select-Object -First 1
if ($first -and $first.Source -ne $target) {
    Write-Host "Warning: $($first.Source) comes first on PATH and runs instead of this install."
    Write-Host "Remove it (for the npm package: npm rm -g @colbymchenry/codegraph) or put $dir first on PATH."
}
Write-Host "Next: run 'codegraph install' to connect Claude Code, Cursor, Codex or opencode."
