#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Destination
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$manifestPath = Join-Path $root 'installer\models\embedding-model.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$destinationPath = [System.IO.Path]::GetFullPath($Destination)
New-Item -ItemType Directory -Force -Path $destinationPath | Out-Null
$maxAttempts = 8

$baseUrl = "https://huggingface.co/$($manifest.source_repository)/resolve/$($manifest.revision)"
foreach ($file in $manifest.files) {
    $target = Join-Path $destinationPath ([string]$file.target)
    if (Test-Path -LiteralPath $target -PathType Leaf) {
        $existingHash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($existingHash -eq [string]$file.sha256) {
            Write-Host "embedding asset already verified: $($file.target)"
            continue
        }
    }

    $partial = "$target.partial"
    Remove-Item -LiteralPath $partial -Force -ErrorAction SilentlyContinue
    $sourcePath = ([string]$file.source).Replace('\', '/')
    $uri = "$baseUrl/$sourcePath`?download=true"
    Write-Host "fetching embedding asset: $($file.target)"
    try {
        $downloaded = $false
        for ($attempt = 1; $attempt -le $maxAttempts; $attempt++) {
            try {
                Invoke-WebRequest `
                    -UseBasicParsing `
                    -Uri $uri `
                    -OutFile $partial `
                    -TimeoutSec 300 `
                    -UserAgent 'Solo-Community/0.12 embedding-model-fetch'
                $downloaded = $true
                break
            }
            catch {
                Remove-Item -LiteralPath $partial -Force -ErrorAction SilentlyContinue
                if ($attempt -eq $maxAttempts) {
                    throw
                }
                $delaySeconds = [Math]::Min(60, [Math]::Pow(2, $attempt - 1))
                Write-Warning "embedding asset fetch attempt $attempt failed; retrying in $delaySeconds seconds"
                Start-Sleep -Seconds $delaySeconds
            }
        }
        if (!$downloaded) {
            throw "embedding asset download did not complete: $($file.target)"
        }
        $actualHash = (Get-FileHash -LiteralPath $partial -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne [string]$file.sha256) {
            throw "SHA-256 mismatch for $($file.target): expected $($file.sha256), got $actualHash"
        }
        Move-Item -LiteralPath $partial -Destination $target -Force
    }
    finally {
        Remove-Item -LiteralPath $partial -Force -ErrorAction SilentlyContinue
    }
}

Copy-Item -LiteralPath $manifestPath -Destination (Join-Path $destinationPath 'embedding-model.json') -Force
Write-Host "packaged embedding model ready: $destinationPath"
