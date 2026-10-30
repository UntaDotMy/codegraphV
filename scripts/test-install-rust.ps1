# Exercise the real Windows install/copy/swap block with local downloads and injected I/O
# failures. The script's PATH/registry changes and network discovery are never executed.
$ErrorActionPreference = 'Stop'
$installScript = Join-Path $PSScriptRoot '..\install-rust.ps1'
$parseTokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Resolve-Path $installScript), [ref]$parseTokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw "Installer parse errors: $parseErrors" }
$block = $ast.EndBlock.Statements | Where-Object { $_ -is [Management.Automation.Language.TryStatementAst] } | Select-Object -First 1
if (-not $block) { throw 'Installer copy/swap block not found' }
$installBlock = [scriptblock]::Create($block.Extent.Text)
$testTempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$testRoot = Join-Path $testTempBase ('codegraphV-install-test-[literal]-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $testRoot | Out-Null

function Invoke-WebRequest {
    param([string]$Uri, [string]$OutFile)
    if ($Uri.EndsWith('/SHA256SUMS')) {
        $hash = (Get-FileHash -LiteralPath (Join-Path $tmp "$name.zip") -Algorithm SHA256).Hash.ToLower()
        [IO.File]::WriteAllText($OutFile, "$hash  $name.zip`n")
    } else {
        [IO.File]::WriteAllText($OutFile, 'local archive payload')
    }
}
function Expand-Archive {
    param([string]$LiteralPath, [string]$DestinationPath)
    $package = Join-Path $DestinationPath $name
    New-Item -ItemType Directory -Path $package | Out-Null
    [IO.File]::WriteAllText((Join-Path $package 'codegraphV.exe'), 'replacement binary')
}
function Copy-Item {
    [CmdletBinding()]
    param([string]$LiteralPath, [string]$Destination)
    if ($scenario -eq 'copy-fails') { throw 'injected copy failure' }
    Microsoft.PowerShell.Management\Copy-Item @PSBoundParameters
}
function Move-Item {
    [CmdletBinding()]
    param([string]$LiteralPath, [string]$Destination, [switch]$Force)
    if ($scenario -eq 'swap-fails' -and $LiteralPath -eq $staged) { throw 'injected swap failure' }
    if ($scenario -eq 'rename-fails' -and $LiteralPath -eq $target) { throw 'injected rename failure' }
    Microsoft.PowerShell.Management\Move-Item @PSBoundParameters
}
try {
    foreach ($scenario in @('copy-fails', 'rename-fails', 'swap-fails', 'success')) {
        $dir = Join-Path $testRoot $scenario
        $tmp = Join-Path $dir 'download'
        if (-not [IO.Path]::GetFullPath($tmp).StartsWith(
            ([IO.Path]::GetFullPath($testRoot) + [IO.Path]::DirectorySeparatorChar),
            [StringComparison]::OrdinalIgnoreCase)) { throw 'Download staging escaped the test root' }
        New-Item -ItemType Directory -Path $tmp -Force | Out-Null
        $target = Join-Path $dir 'codegraphV.exe'
        [IO.File]::WriteAllText($target, 'working original')
        $staged = $null
        $version = 'v9.8.7'
        $name = "codegraphV-$version-windows-x64"
        $base = 'https://example.invalid/local-only'
        $caught = $null
        try { . $installBlock } catch { $caught = $_ }
        if ($scenario -eq 'success') {
            if ($caught) { throw $caught }
            $expected = 'replacement binary'
        } else {
            if (-not $caught -or "$caught" -notlike '*injected*failure*') { throw "Wrong failure for ${scenario}: $caught" }
            $expected = 'working original'
        }
        if ([IO.File]::ReadAllText($target) -ne $expected) { throw "Executable not preserved for $scenario" }
        if (Test-Path -LiteralPath $tmp) { throw "Download staging leaked for $scenario" }
        if (Get-ChildItem -LiteralPath $dir -Force | Where-Object { $_.Name -ne 'codegraphV.exe' }) {
            throw "Swap staging or backup leaked for $scenario"
        }
        Write-Output "${scenario}: passed"
    }
} finally {
    # This is the only recursive cleanup; verify the resolved path is our generated temp root.
    $resolved = [IO.Path]::GetFullPath($testRoot)
    if (-not $resolved.StartsWith($testTempBase, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($resolved) -notlike 'codegraphV-install-test-*') {
        throw "Refusing cleanup outside the test temp directory: $resolved"
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
