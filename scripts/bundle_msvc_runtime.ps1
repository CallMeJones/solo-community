[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ReleaseDir
)

$ErrorActionPreference = 'Stop'
$release = (Resolve-Path -LiteralPath $ReleaseDir).Path
$releaseItem = Get-Item -LiteralPath $release
if (-not $releaseItem.PSIsContainer -or ($releaseItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
    throw "Release directory must be a real directory: $release"
}

$allowedRuntimeName = '^(?:concrt140|msvcp140(?:_[0-9]+|_atomic_wait|_codecvt_ids)?|vccorlib140|vcruntime140(?:_[0-9]+|_threads)?)\.dll$'

function Assert-MicrosoftRuntime([System.IO.FileInfo]$File) {
    if ($File.Name -notmatch $allowedRuntimeName) {
        throw "Unlisted MSVC runtime library: $($File.Name)"
    }
    if (($File.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $File.Length -gt (16 * 1024 * 1024)) {
        throw "MSVC runtime must be a bounded, non-reparse-point file: $($File.FullName)"
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $File.FullName
    if ($signature.Status -ne 'Valid' -or
        $null -eq $signature.SignerCertificate -or
        $signature.SignerCertificate.Subject -notlike '*O=Microsoft Corporation*') {
        throw "MSVC runtime is not validly signed by Microsoft: $($File.FullName)"
    }
}

$searchRoots = @(
    "${env:ProgramFiles}\Microsoft Visual Studio",
    "${env:ProgramFiles(x86)}\Microsoft Visual Studio"
) | Where-Object { Test-Path -LiteralPath $_ -PathType Container }
$crtDirectories = foreach ($root in $searchRoots) {
    Get-ChildItem -LiteralPath $root -Recurse -Filter vcruntime140.dll -ErrorAction SilentlyContinue |
        Where-Object {
            $_.FullName -match '\\VC\\Redist\\MSVC\\[^\\]+\\x64\\Microsoft\.VC\d+\.CRT\\vcruntime140\.dll$'
        } |
        ForEach-Object Directory
}
$crtDirectory = $crtDirectories | Sort-Object FullName -Descending | Select-Object -First 1
if ($null -eq $crtDirectory) {
    throw 'MSVC x64 redistributable CRT directory was not found.'
}
$runtimeFiles = @(Get-ChildItem -LiteralPath $crtDirectory.FullName -File -Filter '*.dll')
if (-not ($runtimeFiles | Where-Object Name -eq 'vcruntime140.dll')) {
    throw "MSVC runtime directory lacks vcruntime140.dll: $($crtDirectory.FullName)"
}
foreach ($runtimeFile in $runtimeFiles) {
    Assert-MicrosoftRuntime $runtimeFile
    Copy-Item -LiteralPath $runtimeFile.FullName -Destination $release -Force
}

$packagedRuntimeFiles = @(Get-ChildItem -LiteralPath $release -File -Filter '*.dll')
foreach ($runtimeFile in $packagedRuntimeFiles) {
    Assert-MicrosoftRuntime $runtimeFile
}
if (-not ($packagedRuntimeFiles | Where-Object Name -eq 'vcruntime140.dll')) {
    throw 'Release directory lacks vcruntime140.dll after bundling.'
}
$packagedRuntimeFiles | Sort-Object Name | Select-Object -ExpandProperty Name
