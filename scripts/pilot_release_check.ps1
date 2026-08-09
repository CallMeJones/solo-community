#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$CoreRoot = '',
    [string]$WebRoot = '',
    [string]$ExpectedBranch = 'pilot',
    [string]$InstallerPath = '',
    [string]$ManifestPath = '',
    [switch]$SkipTests,
    [switch]$AllowDirty,
    [switch]$AllowUnsignedInstaller
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($CoreRoot)) {
    $CoreRoot = Join-Path $PSScriptRoot '..'
}
if ([string]::IsNullOrWhiteSpace($WebRoot)) {
    $WebRoot = Join-Path $PSScriptRoot '..\..\solo-web'
}

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message"
}

function Resolve-ExistingDirectory {
    param([string]$Path, [string]$Label)
    if (!(Test-Path -LiteralPath $Path -PathType Container)) {
        throw "$Label directory does not exist: $Path"
    }
    return (Resolve-Path -LiteralPath $Path).Path
}

function Invoke-Checked {
    param(
        [string]$WorkingDirectory,
        [string]$FileName,
        [string[]]$Arguments
    )

    Push-Location $WorkingDirectory
    try {
        & $FileName @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "$FileName $($Arguments -join ' ') failed with exit code $LASTEXITCODE in $WorkingDirectory"
        }
    }
    finally {
        Pop-Location
    }
}

function Get-RepositoryState {
    param([string]$Path, [string]$Name)

    $safeDirectory = $Path -replace '\\', '/'
    $branch = (& git -c "safe.directory=$safeDirectory" -C $Path branch --show-current).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($branch)) {
        throw "$Name is not on a named Git branch: $Path"
    }
    if ($branch -ne $ExpectedBranch) {
        throw "$Name is on branch '$branch'; expected '$ExpectedBranch'"
    }

    $commit = (& git -c "safe.directory=$safeDirectory" -C $Path rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or $commit -notmatch '^[0-9a-f]{40}$') {
        throw "Could not resolve the $Name commit"
    }

    $dirty = @(& git -c "safe.directory=$safeDirectory" -C $Path status --porcelain=v1 --untracked-files=all)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not read $Name worktree state"
    }
    if (!$AllowDirty -and $dirty.Count -gt 0) {
        throw "$Name worktree is dirty:`n$($dirty -join [Environment]::NewLine)"
    }

    return [ordered]@{
        name   = $Name
        root   = $Path
        branch = $branch
        commit = $commit
        dirty  = [bool]($dirty.Count -gt 0)
    }
}

function Get-TreeDigest {
    param([string]$Path)

    $root = (Resolve-Path -LiteralPath $Path).Path
    $lines = New-Object System.Collections.Generic.List[string]
    foreach ($file in Get-ChildItem -LiteralPath $root -File -Recurse | Sort-Object FullName) {
        $relative = $file.FullName.Substring($root.Length).TrimStart('\', '/') -replace '\\', '/'
        $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $lines.Add("${relative}:${hash}") | Out-Null
    }
    $joined = $lines -join "`n"
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($joined)
        return ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
    }
}

function Write-Utf8NoBom {
    param([string]$Path, [string]$Value)
    $parent = Split-Path -Parent $Path
    if (![string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    $encoding = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($Path, $Value, $encoding)
}

$core = Resolve-ExistingDirectory -Path $CoreRoot -Label 'Solo Core'
$web = Resolve-ExistingDirectory -Path $WebRoot -Label 'Solo Web'

Write-Step 'Verify pinned pilot branches and clean worktrees'
$repositories = @(
    Get-RepositoryState -Path $core -Name 'solo-core'
    Get-RepositoryState -Path $web -Name 'solo-web'
)

$cargoToml = Get-Content -Raw -LiteralPath (Join-Path $core 'Cargo.toml')
if ($cargoToml -notmatch '(?m)^version\s*=\s*"0\.12\.0"\s*$') {
    throw 'Solo Core workspace version is not 0.12.0'
}
if (!(Test-Path -LiteralPath (Join-Path $core 'docs\releases\v0.12.0.md') -PathType Leaf)) {
    throw 'docs/releases/v0.12.0.md is missing'
}

if (!$SkipTests) {
    Write-Step 'Run Solo Core release gates'
    Invoke-Checked -WorkingDirectory $core -FileName 'cargo' -Arguments @('fmt', '--all', '--', '--check')
    Invoke-Checked -WorkingDirectory $core -FileName 'cargo' -Arguments @('test', '--locked', '-p', 'solo-cli', '--no-default-features', '--test', 'community_boundary')
    Invoke-Checked -WorkingDirectory $core -FileName 'cargo' -Arguments @('test', '--locked', '--workspace')
    Invoke-Checked -WorkingDirectory $core -FileName 'cargo' -Arguments @('clippy', '--locked', '--workspace', '--all-targets', '--', '-D', 'warnings')
    Invoke-Checked -WorkingDirectory $core -FileName 'cargo' -Arguments @('build', '--locked', '--release')

    Write-Step 'Run Solo Web release gates'
    Invoke-Checked -WorkingDirectory $web -FileName 'npm.cmd' -Arguments @('run', 'typecheck')
    Invoke-Checked -WorkingDirectory $web -FileName 'npm.cmd' -Arguments @('run', 'lint')
    Invoke-Checked -WorkingDirectory $web -FileName 'npm.cmd' -Arguments @('test')
    Invoke-Checked -WorkingDirectory $web -FileName 'npm.cmd' -Arguments @('run', 'build:pilot')
    Invoke-Checked -WorkingDirectory $web -FileName 'npm.cmd' -Arguments @('audit', '--omit=dev', '--audit-level=high')
}

Write-Step 'Verify embedded Solo Web assets match the candidate Web build'
$webDist = Join-Path $web 'dist'
$embeddedWeb = Join-Path $core 'crates\solo-api\assets\solo-web'
if (!(Test-Path -LiteralPath (Join-Path $webDist 'index.html') -PathType Leaf)) {
    throw "Solo Web dist is missing. Run npm run build in $web"
}
$webDigest = Get-TreeDigest -Path $webDist
$embeddedDigest = Get-TreeDigest -Path $embeddedWeb
if ($webDigest -ne $embeddedDigest) {
    throw "Embedded Web assets do not match solo-web/dist. Run scripts/sync_solo_web_assets.ps1 and rebuild. dist=$webDigest embedded=$embeddedDigest"
}
$webProvenancePath = Join-Path $core 'crates\solo-api\assets\solo-web.provenance.json'
if (!(Test-Path -LiteralPath $webProvenancePath -PathType Leaf)) {
    throw "Embedded Solo Web provenance is missing: $webProvenancePath"
}
$webProvenance = Get-Content -Raw -LiteralPath $webProvenancePath | ConvertFrom-Json
$webRepositoryState = @($repositories | Where-Object { $_.name -eq 'solo-web' })[0]
if ($null -eq $webRepositoryState) {
    throw 'Solo Web repository state is missing from the release manifest'
}
if ($webProvenance.source_commit -ne $webRepositoryState.commit) {
    throw "Embedded Solo Web commit $($webProvenance.source_commit) does not match candidate $($webRepositoryState.commit)"
}
if ($webProvenance.schema_version -ne 2) {
    throw "Embedded Solo Web provenance schema must be 2; found $($webProvenance.schema_version)"
}
if ($webProvenance.source_dirty -ne $false) {
    throw 'Embedded Solo Web provenance records a dirty source tree'
}
if ($webProvenance.dist_tree_sha256 -ne $embeddedDigest) {
    throw "Embedded Solo Web provenance tree hash $($webProvenance.dist_tree_sha256) does not match $embeddedDigest"
}
$candidatePackageLockHash = (Get-FileHash -LiteralPath (Join-Path $web 'package-lock.json') -Algorithm SHA256).Hash.ToLowerInvariant()
if ($webProvenance.package_lock_sha256 -ne $candidatePackageLockHash) {
    throw 'Embedded Solo Web package-lock hash does not match the candidate source tree'
}
if ([string]::IsNullOrWhiteSpace([string]$webProvenance.build_invocation_id)) {
    throw 'Embedded Solo Web provenance lacks a same-invocation build identifier'
}

$installer = $null
if (![string]::IsNullOrWhiteSpace($InstallerPath)) {
    $resolvedInstaller = (Resolve-Path -LiteralPath $InstallerPath).Path
    $signature = Get-AuthenticodeSignature -LiteralPath $resolvedInstaller
    if ($signature.Status -ne 'Valid' -and
        !($AllowUnsignedInstaller -and $signature.Status -eq 'NotSigned')) {
        throw "Installer Authenticode signature is $($signature.Status). The pilot exception permits only NotSigned artifacts, never invalid signatures."
    }
    $installer = [ordered]@{
        path             = $resolvedInstaller
        bytes            = (Get-Item -LiteralPath $resolvedInstaller).Length
        sha256           = (Get-FileHash -LiteralPath $resolvedInstaller -Algorithm SHA256).Hash.ToLowerInvariant()
        signature_status = [string]$signature.Status
        signer_subject   = if ($null -ne $signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { $null }
    }
}

$manifest = [ordered]@{
    schema_version      = 1
    generated_at_utc    = [DateTime]::UtcNow.ToString('o')
    product_scope       = 'windows-community-core-web'
    excluded_components = @('remote-document-upload-required-path')
    repositories        = $repositories
    core_version        = '0.12.0'
    web_tree_sha256     = $webDigest
    embedded_web_sha256 = $embeddedDigest
    web_provenance      = $webProvenance
    installer           = $installer
}

if (![string]::IsNullOrWhiteSpace($ManifestPath)) {
    $resolvedManifest = if ([System.IO.Path]::IsPathRooted($ManifestPath)) {
        $ManifestPath
    }
    else {
        Join-Path $core $ManifestPath
    }
    Write-Utf8NoBom -Path $resolvedManifest -Value ($manifest | ConvertTo-Json -Depth 8)
    Write-Host "release manifest: $resolvedManifest"
}

$manifest | ConvertTo-Json -Depth 8
Write-Step 'Pilot release checks passed'
