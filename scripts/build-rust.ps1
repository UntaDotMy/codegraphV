param(
    [ValidateSet('Debug','Release')][string]$Profile = 'Release',
    [string]$OutputDirectory,
    [switch]$SkipTests
)
$ErrorActionPreference = 'Stop'
$workspaceRoot = Split-Path -Parent $PSScriptRoot
Push-Location $workspaceRoot
try {
    if (-not $SkipTests) {
        cargo test --workspace
        if ($LASTEXITCODE -ne 0) { throw 'Rust workspace tests failed.' }
    }
    $buildArguments = @('build', '-p', 'codegraph-cli')
    $buildProfileDirectory = 'debug'
    if ($Profile -eq 'Release') { $buildArguments += '--release'; $buildProfileDirectory = 'release' }
    cargo @buildArguments
    if ($LASTEXITCODE -ne 0) { throw 'Native CLI build failed.' }
    cargo build -p codegraph-viewer --target wasm32-unknown-unknown --release
    if ($LASTEXITCODE -ne 0) { throw 'Rust/WASM viewer build failed. Install the target with rustup target add wasm32-unknown-unknown.' }
    $wasmBindgen = Join-Path $workspaceRoot 'target/native-tools/bin/wasm-bindgen.exe'
    if (-not (Test-Path -LiteralPath $wasmBindgen)) {
        $foundTool = Get-Command wasm-bindgen -ErrorAction SilentlyContinue
        if ($foundTool) { $wasmBindgen = $foundTool.Source }
        else { throw 'Install the matching wasm-bindgen CLI: cargo install wasm-bindgen-cli --version 0.2.105 --locked --root target/native-tools' }
    }
    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $workspaceRoot 'target/native-bundle' }
    $bundleRoot = [IO.Path]::GetFullPath($OutputDirectory)
    $viewerRoot = Join-Path $bundleRoot 'viewer'
    New-Item -ItemType Directory -Force -Path $viewerRoot | Out-Null
    $binaryName = if ($IsWindows -or $env:OS -eq 'Windows_NT') { 'codegraph.exe' } else { 'codegraph' }
    Copy-Item -LiteralPath (Join-Path $workspaceRoot "target/$buildProfileDirectory/$binaryName") -Destination (Join-Path $bundleRoot $binaryName)
    & $wasmBindgen --target web --no-typescript --out-name codegraph_viewer --out-dir $viewerRoot (Join-Path $workspaceRoot 'target/wasm32-unknown-unknown/release/codegraph_viewer.wasm')
    if ($LASTEXITCODE -ne 0) { throw 'WASM bindings generation failed.' }
    Copy-Item -LiteralPath (Join-Path $workspaceRoot 'crates/codegraph-viewer/index.html') -Destination $viewerRoot
    Copy-Item -LiteralPath (Join-Path $workspaceRoot 'crates/codegraph-viewer/viewer.css') -Destination $viewerRoot
    Copy-Item -LiteralPath (Join-Path $workspaceRoot 'LICENSE') -Destination $bundleRoot
    $artifactFiles = @($binaryName, 'viewer/index.html', 'viewer/viewer.css', 'viewer/codegraph_viewer.js', 'viewer/codegraph_viewer_bg.wasm', 'LICENSE')
    $manifestFiles = foreach ($relativePath in $artifactFiles) {
        $artifactPath = Join-Path $bundleRoot $relativePath
        if (-not (Test-Path -LiteralPath $artifactPath -PathType Leaf)) { throw "Missing release asset: $relativePath" }
        @{ path = $relativePath; sha256 = (Get-FileHash -LiteralPath $artifactPath -Algorithm SHA256).Hash.ToLowerInvariant(); size = (Get-Item -LiteralPath $artifactPath).Length }
    }
    @{ runtime = 'rust'; browser = 'rust-wasm'; profile = $Profile; files = @($manifestFiles) } | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $bundleRoot 'manifest.json') -Encoding utf8
    Write-Output "Native bundle: $bundleRoot"
} finally { Pop-Location }
