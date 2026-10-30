# Exercise install-rust.ps1's version resolution (the "latest" lookup and a pinned version) with the
# network replaced by stand-ins. Nothing is downloaded and nothing outside this process changes.
$ErrorActionPreference = 'Stop'
$installScript = Join-Path $PSScriptRoot '..\install-rust.ps1'
$parseTokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Resolve-Path $installScript), [ref]$parseTokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw "Installer parse errors: $parseErrors" }

$statements = @($ast.EndBlock.Statements)
$lookup = $statements | Where-Object { $_ -is [Management.Automation.Language.FunctionDefinitionAst] -and $_.Name -eq 'Get-LatestTag' } | Select-Object -First 1
if (-not $lookup) { throw 'Get-LatestTag not found in the installer' }
$first = $statements | Where-Object { $_.Extent.Text.StartsWith('$legacy') } | Select-Object -First 1
$last = $statements | Where-Object { $_.Extent.Text.StartsWith('if ($version -notlike') } | Select-Object -First 1
if (-not $first -or -not $last) { throw 'Version resolution statements not found in the installer' }
$resolve = $statements | Where-Object { $_.Extent.StartOffset -ge $first.Extent.StartOffset -and $_.Extent.EndOffset -le $last.Extent.EndOffset }
$resolveBlock = [scriptblock]::Create(($lookup.Extent.Text + "`n" + (($resolve | ForEach-Object { $_.Extent.Text }) -join "`n")))

$script:webCalls = 0
$script:apiCalls = 0
function Invoke-WebRequest {
    param([string]$Uri, [string]$Method, [switch]$UseBasicParsing)
    $script:webCalls++
    if ($scenario -in @('redirect-fails-api-ok', 'both-fail', 'pinned')) { throw 'injected redirect failure' }
    [pscustomobject]@{ BaseResponse = [pscustomobject]@{ ResponseUri = [uri]'https://github.com/UntaDotMy/codegraphV/releases/tag/v1.2.3' } }
}
function Invoke-RestMethod {
    param([string]$Uri, [hashtable]$Headers)
    $script:apiCalls++
    if ($scenario -eq 'redirect-fails-api-ok') { return [pscustomobject]@{ tag_name = 'v4.5.6' } }
    throw 'The remote server returned an error: (403) Forbidden.'
}

$saved = @{}
foreach ($name in 'CODEGRAPHV_VERSION', 'CODEGRAPH_VERSION', 'CODEGRAPH_INSTALL_DIR', 'CODEGRAPHV_INSTALL_DIR') {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    [Environment]::SetEnvironmentVariable($name, $null, 'Process')
}
try {
    $cases = @(
        @{ scenario = 'redirect-ok';           pinned = $null;    expect = 'v1.2.3'; web = 1; api = 0 },
        @{ scenario = 'redirect-fails-api-ok'; pinned = $null;    expect = 'v4.5.6'; web = 1; api = 1 },
        @{ scenario = 'both-fail';             pinned = $null;    expect = $null;    web = 1; api = 1 },
        @{ scenario = 'pinned';                pinned = '7.8.9';  expect = 'v7.8.9'; web = 0; api = 0 }
    )
    foreach ($case in $cases) {
        $scenario = $case.scenario
        $script:webCalls = 0
        $script:apiCalls = 0
        [Environment]::SetEnvironmentVariable('CODEGRAPHV_VERSION', $case.pinned, 'Process')
        $version = $null
        $caught = $null
        try { . $resolveBlock } catch { $caught = $_ }
        if ($null -eq $case.expect) {
            if (-not $caught) { throw "${scenario}: expected the installer to stop, got version '$version'" }
            if ("$caught" -notlike '*Could not find the latest release*') { throw "${scenario}: wrong error: $caught" }
            if ("$caught" -like '*403*') { throw "${scenario}: the raw HTTP error leaked: $caught" }
        } else {
            if ($caught) { throw "${scenario}: unexpected failure: $caught" }
            if ($version -ne $case.expect) { throw "${scenario}: version '$version', expected '$($case.expect)'" }
        }
        if ($script:webCalls -ne $case.web) { throw "${scenario}: $($script:webCalls) redirect lookups, expected $($case.web)" }
        if ($script:apiCalls -ne $case.api) { throw "${scenario}: $($script:apiCalls) API calls, expected $($case.api)" }
        Write-Output "${scenario}: passed"
    }
} finally {
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
