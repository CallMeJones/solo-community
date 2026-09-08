param(
    [ValidateRange(1, 20)][int]$Attempts = 3,
    [string]$TestBinary,
    [string]$OutputDirectory = 'target/windows-api-diagnostics',
    [string]$ProcDumpPath
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This diagnostic runs on Windows.' }
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$outputRoot = (Resolve-Path -LiteralPath $OutputDirectory).Path

if (-not $TestBinary) {
    $buildErrors = Join-Path $outputRoot 'build-errors.log'
    $messages = @(& cargo test --locked -p solo-api --no-default-features --no-run --message-format=json 2> $buildErrors)
    if ($LASTEXITCODE -ne 0) { throw "API test build failed; inspect $buildErrors" }
    $artifacts = @($messages | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object {
        $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'solo_api' -and $_.profile.test -and $_.executable
    })
    if ($artifacts.Count -ne 1) { throw 'Expected exactly one solo-api unit-test executable.' }
    $TestBinary = $artifacts[0].executable
}
$TestBinary = (Resolve-Path -LiteralPath $TestBinary).Path

if (-not $ProcDumpPath) {
    $toolRoot = Join-Path $outputRoot 'tools'
    New-Item -ItemType Directory -Force -Path $toolRoot | Out-Null
    $archive = Join-Path $toolRoot 'Procdump.zip'
    Invoke-WebRequest 'https://download.sysinternals.com/files/Procdump.zip' -OutFile $archive
    # Microsoft ProcDump 12.01, verified 2026-09-08. A changed download fails
    # closed until its release and hash have been reviewed.
    $expected = '68e057587b0fd654efa095f76d80d633c0e5c60ea26fd3e7c0011c076bb2d00c'
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected) {
        throw 'ProcDump archive digest changed; review the upstream release before updating the pin.'
    }
    Expand-Archive -LiteralPath $archive -DestinationPath $toolRoot -Force
    $ProcDumpPath = Join-Path $toolRoot 'procdump64.exe'
}
$signature = Get-AuthenticodeSignature -LiteralPath $ProcDumpPath
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|, )O=Microsoft Corporation(,|$)') {
    throw 'ProcDump must have a valid Microsoft Authenticode signature.'
}

for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
    $attemptRoot = Join-Path $outputRoot "attempt-$attempt"
    New-Item -ItemType Directory -Force -Path $attemptRoot | Out-Null
    # Scope the monitor to this synthetic test process; never install a system
    # debugger or attach to the user's running Solo library.
    $monitorOutput = (& $ProcDumpPath -accepteula -e -x $attemptRoot $TestBinary --quiet 2>&1 | Out-String).Replace([string][char]0, '')
    $monitorExit = $LASTEXITCODE
    $monitorOutput | Set-Content -LiteralPath (Join-Path $attemptRoot 'api.log') -Encoding utf8
    $exitMatch = [regex]::Match($monitorOutput, 'Process Exit: PID\s+\d+, Exit Code\s+0x([0-9a-fA-F]+)')
    $testsPassed = $monitorOutput -match 'test result: ok\. [1-9][0-9]* passed; 0 failed;'
    if ($monitorExit -ne 0 -or -not $exitMatch.Success -or
        [Convert]::ToUInt32($exitMatch.Groups[1].Value, 16) -ne 0 -or -not $testsPassed) {
        throw "API process attempt $attempt failed or produced incomplete evidence; inspect $attemptRoot"
    }
    Write-Output "API process attempt $attempt passed, including native process exit."
}
