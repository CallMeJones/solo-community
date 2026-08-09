#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$WebDist = '',
    [string]$AssetDir = '',
    [string]$ProvenancePath = '',
    [switch]$AllowDirtyWebSource
)

$ErrorActionPreference = 'Stop'

# Windows PowerShell 5.1 does not reliably populate $PSScriptRoot while
# evaluating parameter default expressions. Resolve script-relative defaults
# after binding so the release helper works in both powershell.exe and pwsh.
if ([string]::IsNullOrWhiteSpace($WebDist)) {
    $WebDist = Join-Path $PSScriptRoot '..\..\solo-web\dist'
}
if ([string]::IsNullOrWhiteSpace($AssetDir)) {
    $AssetDir = Join-Path $PSScriptRoot '..\crates\solo-api\assets\solo-web'
}
if ([string]::IsNullOrWhiteSpace($ProvenancePath)) {
    $ProvenancePath = Join-Path $PSScriptRoot '..\crates\solo-api\assets\solo-web.provenance.json'
}

function Get-CanonicalPath {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) {
        return (Resolve-Path -LiteralPath $Path).Path.TrimEnd('\', '/')
    }
    return [System.IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
}

function Test-PathIsSameOrChild {
    param([string]$Path, [string]$Root)
    $candidate = Get-CanonicalPath -Path $Path
    $rootPath = Get-CanonicalPath -Path $Root
    if ([string]::Equals($candidate, $rootPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $true
    }
    $prefix = $rootPath.TrimEnd('\', '/') + [System.IO.Path]::DirectorySeparatorChar
    return $candidate.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)
}

function Get-TreeDigest {
    param([string]$Root)
    $digestLines = New-Object System.Collections.Generic.List[string]
    foreach ($file in Get-ChildItem -LiteralPath $Root -File -Recurse | Sort-Object FullName) {
        $relative = $file.FullName.Substring($Root.Length).TrimStart('\', '/') -replace '\\', '/'
        $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        $digestLines.Add("${relative}:${hash}") | Out-Null
    }
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $treeBytes = [System.Text.Encoding]::UTF8.GetBytes(($digestLines -join "`n"))
        return ([BitConverter]::ToString($sha.ComputeHash($treeBytes))).Replace('-', '').ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
    }
}

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$webDistPath = Get-CanonicalPath -Path $WebDist
$webRoot = (Resolve-Path -LiteralPath (Split-Path -Parent $webDistPath)).Path
$assetPath = Get-CanonicalPath -Path $AssetDir
$provenanceFullPath = Get-CanonicalPath -Path $ProvenancePath
$expectedAssetRoot = (Resolve-Path -LiteralPath (Join-Path $repoRoot 'crates\solo-api\assets')).Path

if (!(Test-PathIsSameOrChild -Path $assetPath -Root $expectedAssetRoot)) {
    throw "Refusing to sync assets outside ${expectedAssetRoot}: $assetPath"
}
if (!(Test-PathIsSameOrChild -Path $provenanceFullPath -Root $expectedAssetRoot)) {
    throw "Refusing to write provenance outside ${expectedAssetRoot}: $provenanceFullPath"
}
if ((Test-PathIsSameOrChild -Path $assetPath -Root $webDistPath) -or
    (Test-PathIsSameOrChild -Path $webDistPath -Root $assetPath)) {
    throw "Refusing overlapping Solo Web source/target trees: source=$webDistPath target=$assetPath"
}
if (Test-PathIsSameOrChild -Path $provenanceFullPath -Root $assetPath) {
    throw "Provenance must remain outside the replaceable asset tree: $provenanceFullPath"
}

$safeWebRoot = $webRoot -replace '\\', '/'
$webCommit = (& git -c "safe.directory=$safeWebRoot" -C $webRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $webCommit -notmatch '^[0-9a-f]{40}$') {
    throw "Could not resolve the Solo Web source commit in $webRoot"
}
$webDirty = @(& git -c "safe.directory=$safeWebRoot" -C $webRoot status --porcelain=v1 --untracked-files=all)
if ($LASTEXITCODE -ne 0) {
    throw "Could not inspect the Solo Web worktree in $webRoot"
}
if (!$AllowDirtyWebSource -and $webDirty.Count -gt 0) {
    throw "Solo Web source is dirty; commit the candidate before embedding it:`n$($webDirty -join [Environment]::NewLine)"
}
$packageLock = Join-Path $webRoot 'package-lock.json'
if (!(Test-Path -LiteralPath $packageLock -PathType Leaf)) {
    throw "Solo Web package-lock.json is missing: $packageLock"
}

# Build the inspected source in this invocation. Provenance can therefore not
# be stamped onto a stale dist left by an earlier checkout or failed build.
$npm = Get-Command npm.cmd -ErrorAction SilentlyContinue
if ($null -eq $npm) {
    $npm = Get-Command npm -ErrorAction Stop
}
$buildInvocationId = [guid]::NewGuid().ToString('N')
$buildStartedAt = [DateTime]::UtcNow.ToString('o')
Push-Location $webRoot
try {
    & $npm.Source ci --no-audit --no-fund
    if ($LASTEXITCODE -ne 0) {
        throw "npm ci failed with exit code $LASTEXITCODE"
    }
    & $npm.Source run build:pilot
    if ($LASTEXITCODE -ne 0) {
        throw "npm run build:pilot failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

$commitAfterBuild = (& git -c "safe.directory=$safeWebRoot" -C $webRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $commitAfterBuild -ne $webCommit) {
    throw "Solo Web HEAD changed during build: before=$webCommit after=$commitAfterBuild"
}
$webDirtyAfterBuild = @(& git -c "safe.directory=$safeWebRoot" -C $webRoot status --porcelain=v1 --untracked-files=all)
if ($LASTEXITCODE -ne 0) {
    throw "Could not inspect the Solo Web worktree after the build in $webRoot"
}
if (!$AllowDirtyWebSource -and $webDirtyAfterBuild.Count -gt 0) {
    throw "Solo Web build mutated the clean candidate worktree:`n$($webDirtyAfterBuild -join [Environment]::NewLine)"
}
$sourceDirty = [bool]($webDirty.Count -gt 0 -or $webDirtyAfterBuild.Count -gt 0)
$webDistPath = (Resolve-Path -LiteralPath $webDistPath).Path
if (!(Test-Path -LiteralPath (Join-Path $webDistPath 'index.html') -PathType Leaf)) {
    throw "Fresh Solo Web build did not produce index.html in '$webDistPath'"
}
$sourceTreeDigest = Get-TreeDigest -Root $webDistPath

$assetParent = Split-Path -Parent $assetPath
New-Item -ItemType Directory -Force -Path $assetParent | Out-Null
$suffix = [guid]::NewGuid().ToString('N')
$stagePath = Join-Path $assetParent ".solo-web-stage-$suffix"
$assetBackup = Join-Path $assetParent ".solo-web-backup-$suffix"
$provenanceTemp = "$provenanceFullPath.tmp-$suffix"
$provenanceBackup = "$provenanceFullPath.bak-$suffix"

try {
New-Item -ItemType Directory -Path $stagePath | Out-Null
Get-ChildItem -LiteralPath $webDistPath -Force | Copy-Item -Destination $stagePath -Recurse -Force
$stagedTreeDigest = Get-TreeDigest -Root $stagePath
if ($stagedTreeDigest -ne $sourceTreeDigest) {
    throw "Staged asset digest does not match the fresh build: source=$sourceTreeDigest staged=$stagedTreeDigest"
}

$provenance = [ordered]@{
    schema_version       = 2
    source_repository   = 'CallMeJones/solo-web-community'
    source_commit       = $webCommit
    source_dirty        = $sourceDirty
    package_lock_sha256 = (Get-FileHash -LiteralPath $packageLock -Algorithm SHA256).Hash.ToLowerInvariant()
    dist_tree_sha256    = $stagedTreeDigest
    build_invocation_id = $buildInvocationId
    build_started_at_utc = $buildStartedAt
    build_completed_at_utc = [DateTime]::UtcNow.ToString('o')
    build_commands      = @('npm ci --no-audit --no-fund', 'npm run build:pilot')
}
$json = $provenance | ConvertTo-Json -Depth 6
[System.IO.File]::WriteAllText($provenanceTemp, $json, [System.Text.UTF8Encoding]::new($false))

$movedOldAssets = $false
$movedOldProvenance = $false
$installedNewAssets = $false
$installedNewProvenance = $false
try {
    if (Test-Path -LiteralPath $assetPath) {
        Move-Item -LiteralPath $assetPath -Destination $assetBackup
        $movedOldAssets = $true
    }
    if (Test-Path -LiteralPath $provenanceFullPath) {
        Move-Item -LiteralPath $provenanceFullPath -Destination $provenanceBackup
        $movedOldProvenance = $true
    }
    Move-Item -LiteralPath $stagePath -Destination $assetPath
    $installedNewAssets = $true
    Move-Item -LiteralPath $provenanceTemp -Destination $provenanceFullPath
    $installedNewProvenance = $true

    $installedTreeDigest = Get-TreeDigest -Root $assetPath
    if ($installedTreeDigest -ne $sourceTreeDigest) {
        throw "Installed asset digest does not match the fresh build: source=$sourceTreeDigest installed=$installedTreeDigest"
    }
}
catch {
    if ($installedNewProvenance -and (Test-Path -LiteralPath $provenanceFullPath)) {
        Remove-Item -LiteralPath $provenanceFullPath -Force
    }
    if ($installedNewAssets -and (Test-Path -LiteralPath $assetPath)) {
        Remove-Item -LiteralPath $assetPath -Recurse -Force
    }
    if ($movedOldProvenance -and (Test-Path -LiteralPath $provenanceBackup)) {
        Move-Item -LiteralPath $provenanceBackup -Destination $provenanceFullPath
    }
    if ($movedOldAssets -and (Test-Path -LiteralPath $assetBackup)) {
        Move-Item -LiteralPath $assetBackup -Destination $assetPath
    }
    throw
}
}
finally {
    if (Test-Path -LiteralPath $stagePath) {
        Remove-Item -LiteralPath $stagePath -Recurse -Force
    }
    if (Test-Path -LiteralPath $provenanceTemp) {
        Remove-Item -LiteralPath $provenanceTemp -Force
    }
}

# Backup disposal is deliberately outside the rollback transaction. Once the
# new assets and provenance have both been installed and verified, a cleanup
# failure must leave that valid pair in place rather than deleting it after an
# old backup has been partially removed.
$cleanupErrors = New-Object System.Collections.Generic.List[string]
if ($movedOldAssets -and (Test-Path -LiteralPath $assetBackup)) {
    try {
        Remove-Item -LiteralPath $assetBackup -Recurse -Force
    }
    catch {
        $cleanupErrors.Add("asset backup ${assetBackup}: $($_.Exception.Message)") | Out-Null
    }
}
if ($movedOldProvenance -and (Test-Path -LiteralPath $provenanceBackup)) {
    try {
        Remove-Item -LiteralPath $provenanceBackup -Force
    }
    catch {
        $cleanupErrors.Add("provenance backup ${provenanceBackup}: $($_.Exception.Message)") | Out-Null
    }
}
if ($cleanupErrors.Count -gt 0) {
    throw "Fresh Solo Web assets were installed, but backup cleanup failed; the valid installed pair was preserved:`n$($cleanupErrors -join [Environment]::NewLine)"
}

Write-Host "Synced freshly built solo-web assets"
Write-Host "  from: $webDistPath"
Write-Host "  to:   $assetPath"
Write-Host "  source commit: $webCommit"
Write-Host "  tree SHA-256: $stagedTreeDigest"
Write-Host "  build invocation: $buildInvocationId"
Write-Host "  provenance: $provenanceFullPath"
