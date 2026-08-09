#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Solo'),
    [string]$DataDir = '',
    [int]$Port = 0,
    [string]$Passphrase = 'solo-installed-smoke-passphrase',
    [int]$ExpectedToolCount = 39,
    [int]$TimeoutSeconds = 30,
    [switch]$AllowDestructiveExistingTestDataDir,
    [switch]$SkipRestoreDrill,
    [switch]$DesktopWindowSmoke,
    [int]$DesktopWindowTimeoutSeconds = 10,
    [switch]$DesktopClickSmoke,
    [int]$DesktopClickSmokeTimeoutSeconds = 20
)

$ErrorActionPreference = 'Stop'

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message"
}

function ConvertTo-JsonBody {
    param([hashtable]$Value)
    return ($Value | ConvertTo-Json -Depth 16 -Compress)
}

function Invoke-Json {
    param(
        [string]$Method,
        [string]$Uri,
        [hashtable]$Body = $null,
        [hashtable]$Headers = @{}
    )

    $args = @{
        Method      = $Method
        Uri         = $Uri
        Headers     = $Headers
        ErrorAction = 'Stop'
    }
    if ($null -ne $Body) {
        $args['ContentType'] = 'application/json'
        $args['Body'] = ConvertTo-JsonBody $Body
    }
    return Invoke-RestMethod @args
}

function Wait-ForStatus {
    param(
        [string]$BaseUrl,
        [int]$TimeoutSeconds,
        [System.Diagnostics.Process]$DaemonProcess
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $lastError = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($null -eq $DaemonProcess) {
            throw 'Installed daemon process was not created'
        }
        $DaemonProcess.Refresh()
        if ($DaemonProcess.HasExited) {
            throw "Installed daemon exited before readiness with code $($DaemonProcess.ExitCode)"
        }
        try {
            $status = Invoke-Json -Method Get -Uri "$BaseUrl/v1/status"
            if ($status.ok -eq $true) {
                return $status
            }
        }
        catch {
            $lastError = $_.Exception.Message
        }
        Start-Sleep -Milliseconds 500
    }
    throw "Timed out waiting for $BaseUrl/v1/status. Last error: $lastError"
}

function Get-CanonicalPath {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) {
        return (Resolve-Path -LiteralPath $Path).Path.TrimEnd('\', '/')
    }
    return [System.IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
}

function Test-SamePath {
    param([string]$Left, [string]$Right)
    return [string]::Equals(
        (Get-CanonicalPath -Path $Left),
        (Get-CanonicalPath -Path $Right),
        [System.StringComparison]::OrdinalIgnoreCase)
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

function Reserve-SmokePort {
    param([int]$RequestedPort)
    if ($RequestedPort -lt 0 -or $RequestedPort -gt 65535) {
        throw "Port must be 0 (automatic) or 1..65535; got $RequestedPort"
    }
    $listener = [System.Net.Sockets.TcpListener]::new(
        [System.Net.IPAddress]::Loopback,
        $RequestedPort)
    try {
        $listener.Start()
    }
    catch {
        $listener.Stop()
        throw "Requested smoke port $RequestedPort is already in use or unavailable: $($_.Exception.Message)"
    }
    return $listener
}

function Assert-InstalledBinaryVersion {
    param([string]$Binary, [string]$Label)

    # solo-tray.exe is built as a Windows GUI-subsystem process. PowerShell's
    # call operator does not wait for or capture redirected output from that
    # process type, so `@(& $Binary --version 2>&1)` can return an empty result
    # even though the binary prints a valid version. Use Process with explicit
    # redirection and a bounded wait for both console and GUI-subsystem builds.
    $processInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $processInfo.FileName = $Binary
    $processInfo.Arguments = '--version'
    $processInfo.UseShellExecute = $false
    $processInfo.CreateNoWindow = $true
    $processInfo.RedirectStandardOutput = $true
    $processInfo.RedirectStandardError = $true

    $process = [System.Diagnostics.Process]::Start($processInfo)
    if ($null -eq $process) {
        throw "$Label --version process could not be started"
    }
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit(10000)) {
            $process.Kill()
            [void]$process.WaitForExit(5000)
            throw "$Label --version did not exit within 10 seconds"
        }
        $output = @(
            $stdout.GetAwaiter().GetResult()
            $stderr.GetAwaiter().GetResult()
        ) | Where-Object { ![string]::IsNullOrWhiteSpace([string]$_) }
        $exitCode = $process.ExitCode
    }
    finally {
        $process.Dispose()
    }

    if ($exitCode -ne 0) {
        throw "$Label --version exited with code $exitCode`: $($output -join ' ')"
    }
    $text = ($output -join ' ').Trim()
    $versionToken = @($text -split '\s+')[-1]
    if ($versionToken -notmatch '^0\.12\.0(?:\+[0-9A-Za-z.-]+)?$') {
        throw "$Label semantic version must be exactly 0.12.0 (optional build metadata allowed); got '$text'"
    }
    Write-Host "$Label version ok: $text"
}

function Header-Value {
    param($Headers, [string]$Name)
    $value = $Headers[$Name]
    if ($value -is [array]) {
        return $value[0]
    }
    return $value
}

function Quote-ProcessArgument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') {
        return $Value
    }
    return '"' + ($Value -replace '"', '\"') + '"'
}

function Invoke-SoloWithPassphrase {
    param(
        [string]$Solo,
        [string]$Passphrase,
        [string[]]$Arguments
    )

    $processInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $processInfo.FileName = $Solo
    $processInfo.Arguments = ($Arguments | ForEach-Object { Quote-ProcessArgument $_ }) -join ' '
    $processInfo.UseShellExecute = $false
    $processInfo.CreateNoWindow = $true
    $processInfo.RedirectStandardOutput = $true
    $processInfo.RedirectStandardError = $true
    $oldProcessPassphrase = $env:SOLO_PASSPHRASE
    try {
        $env:SOLO_PASSPHRASE = $Passphrase
        try {
            $process = [System.Diagnostics.Process]::Start($processInfo)
        }
        finally {
            if ($null -eq $oldProcessPassphrase) {
                Remove-Item Env:\SOLO_PASSPHRASE -ErrorAction SilentlyContinue
            }
            else {
                $env:SOLO_PASSPHRASE = $oldProcessPassphrase
            }
        }
        if ($null -eq $process) {
            throw "solo $($Arguments -join ' ') process could not be started"
        }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (!$process.WaitForExit(60000)) {
            $process.Kill()
            [void]$process.WaitForExit(5000)
            throw "solo $($Arguments -join ' ') did not exit within 60 seconds"
        }
        $output = @(
            $stdout.GetAwaiter().GetResult()
            $stderr.GetAwaiter().GetResult()
        ) | Where-Object { ![string]::IsNullOrWhiteSpace([string]$_) }
        $exitCode = $process.ExitCode
    }
    finally {
        if ($null -ne $process) {
            $process.Dispose()
        }
    }

    if ($exitCode -ne 0) {
        throw "solo $($Arguments -join ' ') failed with exit code ${exitCode}:`n$($output -join [Environment]::NewLine)"
    }
    return $output
}

function Invoke-RawPatch {
    param(
        [string]$Uri,
        [byte[]]$Bytes,
        [hashtable]$Headers
    )

    Add-Type -AssemblyName System.Net.Http
    $client = [System.Net.Http.HttpClient]::new()
    $request = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::new('PATCH'), $Uri)
    $request.Content = [System.Net.Http.ByteArrayContent]::new($Bytes)
    try {
        foreach ($entry in $Headers.GetEnumerator()) {
            if (!$request.Headers.TryAddWithoutValidation([string]$entry.Key, [string]$entry.Value)) {
                [void]$request.Content.Headers.TryAddWithoutValidation([string]$entry.Key, [string]$entry.Value)
            }
        }
        $response = $client.SendAsync($request).GetAwaiter().GetResult()
        $detail = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
        if (!$response.IsSuccessStatusCode) {
            throw "PATCH $Uri failed with HTTP $([int]$response.StatusCode): $detail"
        }
    }
    finally {
        if ($null -ne $response) {
            $response.Dispose()
        }
        $request.Dispose()
        $client.Dispose()
    }
}

function Test-StagedDocumentLifecycle {
    param(
        [string]$BaseUrl,
        [string]$DataDir,
        [long]$Stamp
    )

    $filename = "solo-pilot-upload-$Stamp.md"
    $marker = "solo-pilot-upload-marker-$Stamp-$([guid]::NewGuid().ToString('N'))"
    $source = "# Solo pilot provenance`n`nBrowser upload marker $marker remains source-consistent."
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($source)
    $prepared = Invoke-Json -Method Post -Uri "$BaseUrl/memory/documents/uploads" -Body @{
        filename   = $filename
        mime_type  = 'text/markdown'
        size_bytes = $bytes.Length
    }
    if ([string]::IsNullOrWhiteSpace([string]$prepared.upload_id)) {
        throw 'document upload prepare did not return upload_id'
    }

    $uploadUri = if ([Uri]::IsWellFormedUriString([string]$prepared.upload_url, [UriKind]::Absolute)) {
        [string]$prepared.upload_url
    }
    else {
        [Uri]::new([Uri]($BaseUrl.TrimEnd('/') + '/'), [string]$prepared.upload_url).AbsoluteUri
    }
    $uploadHeaders = @{}
    foreach ($property in $prepared.required_headers.PSObject.Properties) {
        $uploadHeaders[[string]$property.Name] = [string]$property.Value
    }
    $offsetHeader = if ([string]::IsNullOrWhiteSpace([string]$prepared.upload_offset_header)) {
        'upload-offset'
    }
    else {
        [string]$prepared.upload_offset_header
    }
    $lengthHeader = if ([string]::IsNullOrWhiteSpace([string]$prepared.upload_length_header)) {
        'upload-length'
    }
    else {
        [string]$prepared.upload_length_header
    }
    $uploadHeaders[$offsetHeader] = '0'
    $uploadHeaders[$lengthHeader] = [string]$bytes.Length
    Invoke-RawPatch -Uri $uploadUri -Bytes $bytes -Headers $uploadHeaders

    $committed = Invoke-Json -Method Post -Uri "$BaseUrl/memory/documents/uploads/$($prepared.upload_id)/commit" -Body @{}
    if ([long]$committed.size_bytes -ne $bytes.Length -or
        [string]::IsNullOrWhiteSpace([string]$committed.staged_uri)) {
        throw 'document upload commit returned invalid size or staged_uri'
    }
    $ingested = Invoke-Json -Method Post -Uri "$BaseUrl/memory/documents/staged/ingest" -Body @{
        staged_uri         = [string]$committed.staged_uri
        retain_source_file = $false
        store_original_file = $true
    }
    if ($ingested.extraction_status -ne 'extracted' -or [int]$ingested.chunks_persisted -lt 1) {
        throw "staged document was not searchable: status=$($ingested.extraction_status), chunks=$($ingested.chunks_persisted), error=$($ingested.extraction_error)"
    }
    if ($null -eq $ingested.asset -or $null -eq $ingested.document_asset_link) {
        throw 'staged document did not retain an asset and provenance link'
    }
    if ($ingested.document_asset_link.doc_id -ne $ingested.document_id -or
        $ingested.document_asset_link.asset_id -ne $ingested.asset.asset_id) {
        throw 'document provenance link does not match the document and retained asset'
    }
    if ($ingested.deleted_staged_file -ne $true) {
        throw 'successful staged ingest did not delete its temporary upload bytes'
    }

    $storageRelative = ([string]$ingested.asset.storage_path) -replace '/', '\'
    $assetBlobPath = Join-Path $DataDir $storageRelative
    if (!(Test-Path -LiteralPath $assetBlobPath -PathType Leaf)) {
        throw "retained source blob is missing: $assetBlobPath"
    }

    $hitsBeforeForget = @(Invoke-Json -Method Post -Uri "$BaseUrl/memory/documents/search" -Body @{
        query = $marker
        limit = 10
    })
    if (!($hitsBeforeForget | Where-Object { $_.doc_id -eq $ingested.document_id })) {
        throw "unique upload marker was not searchable before forget: $marker"
    }

    $forgottenDocument = Invoke-Json -Method Delete -Uri "$BaseUrl/memory/documents/$($ingested.document_id)"
    if ($forgottenDocument.doc_id -ne $ingested.document_id -or [int]$forgottenDocument.chunks_tombstoned -lt 1) {
        throw 'document soft-forget did not tombstone its searchable chunks'
    }
    $hitsAfterForget = @(Invoke-Json -Method Post -Uri "$BaseUrl/memory/documents/search" -Body @{
        query = $marker
        limit = 10
    })
    if ($hitsAfterForget | Where-Object { $_.doc_id -eq $ingested.document_id }) {
        throw "forgotten document remained searchable by its unique marker: $marker"
    }

    Write-Host "staged document lifecycle ok: doc=$($ingested.document_id), asset=$($ingested.asset.asset_id), marker removed from search"
    return [pscustomobject]@{
        DocumentId   = [string]$ingested.document_id
        AssetId      = [string]$ingested.asset.asset_id
        AssetBlobPath = $assetBlobPath
        Marker       = $marker
    }
}

function Add-DesktopWindowProbe {
    if (([System.Management.Automation.PSTypeName]'SoloSmoke.NativeWindow').Type) {
        return
    }

    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;

namespace SoloSmoke
{
    public static class NativeWindow
    {
        private delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

        [DllImport("user32.dll")]
        private static extern bool EnumWindows(EnumWindowsProc enumProc, IntPtr lParam);

        [DllImport("user32.dll")]
        private static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);

        [DllImport("user32.dll")]
        private static extern bool IsWindowVisible(IntPtr hWnd);

        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        private static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int maxCount);

        public static bool HasVisibleTitledWindow(uint targetProcessId, string title)
        {
            bool found = false;
            EnumWindows((hWnd, lParam) =>
            {
                uint processId;
                GetWindowThreadProcessId(hWnd, out processId);
                if (processId != targetProcessId || !IsWindowVisible(hWnd))
                {
                    return true;
                }

                var text = new StringBuilder(256);
                int length = GetWindowText(hWnd, text, text.Capacity);
                if (length > 0 && text.ToString().Equals(title, StringComparison.Ordinal))
                {
                    found = true;
                    return false;
                }
                return true;
            }, IntPtr.Zero);
            return found;
        }
    }
}
'@
}

function Wait-ForDesktopWindow {
    param(
        [System.Diagnostics.Process]$Process,
        [int]$TimeoutSeconds
    )

    Add-DesktopWindowProbe
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($Process.HasExited) {
            throw "Solo window process exited early with code $($Process.ExitCode)"
        }
        if ([SoloSmoke.NativeWindow]::HasVisibleTitledWindow([uint32]$Process.Id, 'Solo')) {
            return
        }
        Start-Sleep -Milliseconds 250
    }
    throw "Timed out waiting for Solo window for process $($Process.Id)"
}

function Stop-SmokeProcess {
    param([System.Diagnostics.Process]$Process)
    if ($null -eq $Process -or $Process.HasExited) {
        return
    }
    [void]$Process.CloseMainWindow()
    if (!$Process.WaitForExit(5000)) {
        $Process.Kill()
        [void]$Process.WaitForExit(5000)
    }
}

function Start-DesktopWindowProcess {
    param(
        [string]$Tray,
        [string]$DesktopUrl,
        [string]$RouteFile = '',
        [string]$SmokeReportFile = ''
    )

    $processInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $processInfo.FileName = $Tray
    $processInfo.UseShellExecute = $false
    $processInfo.CreateNoWindow = $false
    $desktopArgs = @('--desktop-window', '--desktop-url', $DesktopUrl)
    if (![string]::IsNullOrWhiteSpace($RouteFile)) {
        $desktopArgs += @('--desktop-route-file', $RouteFile)
    }
    if (![string]::IsNullOrWhiteSpace($SmokeReportFile)) {
        $desktopArgs += @('--desktop-smoke-report', $SmokeReportFile)
    }
    $processInfo.Arguments = ($desktopArgs | ForEach-Object { Quote-ProcessArgument $_ }) -join ' '
    return [System.Diagnostics.Process]::Start($processInfo)
}

function Start-DesktopWindowSmoke {
    param(
        [string]$Tray,
        [string]$DesktopUrl,
        [int]$TimeoutSeconds
    )

    $desktop = Start-DesktopWindowProcess -Tray $Tray -DesktopUrl $DesktopUrl
    try {
        Wait-ForDesktopWindow -Process $desktop -TimeoutSeconds $TimeoutSeconds
        Write-Host "desktop window ok: pid=$($desktop.Id), url=$DesktopUrl"
    }
    finally {
        Stop-SmokeProcess -Process $desktop
    }
}

function Read-DesktopSmokeReports {
    param([string]$ReportFile)

    if (!(Test-Path -LiteralPath $ReportFile)) {
        return @()
    }
    $reports = New-Object System.Collections.Generic.List[object]
    foreach ($line in Get-Content -LiteralPath $ReportFile -ErrorAction SilentlyContinue) {
        if ([string]::IsNullOrWhiteSpace($line)) {
            continue
        }
        try {
            $reports.Add(($line | ConvertFrom-Json)) | Out-Null
        }
        catch {
            # The webview may still be appending the current line. The next poll
            # will read it again once the write is complete.
        }
    }
    return $reports.ToArray()
}

function Wait-ForDesktopSmokeReport {
    param(
        [string]$ReportFile,
        [string]$ExpectedHash,
        [string[]]$RequiredText,
        [int]$TimeoutSeconds
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $lastReport = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        $reports = @(Read-DesktopSmokeReports -ReportFile $ReportFile)
        foreach ($report in @($reports | Select-Object -Last 20)) {
            if ($report.kind -ne 'solo-desktop-smoke' -or $report.hash -ne $ExpectedHash) {
                continue
            }
            $lastReport = $report
            $bodyText = [string]$report.bodyText
            $missing = @($RequiredText | Where-Object { $bodyText -notlike "*$_*" })
            if (@('Solo', 'Solo Memory') -contains $report.title -and $missing.Count -eq 0) {
                return $report
            }
        }
        Start-Sleep -Milliseconds 250
    }

    $lastDetail = if ($null -eq $lastReport) {
        'no matching report'
    }
    else {
        "last matching title=$($lastReport.title), readyState=$($lastReport.readyState)"
    }
    throw "Timed out waiting for Desktop smoke report $ExpectedHash ($lastDetail)"
}

function Join-DesktopRouteUrl {
    param(
        [string]$DesktopUrl,
        [string]$Route
    )

    $base = $DesktopUrl -replace '#.*$', ''
    $routeHash = $Route.TrimStart('#')
    return "$base#$routeHash"
}

function Start-DesktopClickSmoke {
    param(
        [string]$Tray,
        [string]$DesktopUrl,
        [string]$DataDir,
        [int]$WindowTimeoutSeconds,
        [int]$RouteTimeoutSeconds
    )

    $routeFile = Join-Path $DataDir 'desktop-click-smoke-route.txt'
    $reportFile = Join-Path $DataDir 'desktop-click-smoke-report.jsonl'
    Remove-Item -LiteralPath $routeFile -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $reportFile -Force -ErrorAction SilentlyContinue

    $desktop = Start-DesktopWindowProcess `
        -Tray $Tray `
        -DesktopUrl $DesktopUrl `
        -RouteFile $routeFile `
        -SmokeReportFile $reportFile
    try {
        Wait-ForDesktopWindow -Process $desktop -TimeoutSeconds $WindowTimeoutSeconds
        $pages = @(
            @{ Route = 'setup'; Text = @('Setup', 'First Run', 'Readiness') },
            @{ Route = 'health'; Text = @('Health', 'Daemon State', 'MCP Status') },
            @{ Route = 'connections'; Text = @('Connections', 'Solo MCP', 'Connected Clients', 'Memory Policy') },
            @{ Route = 'profiles'; Text = @('Profiles', 'Profile Databases', 'Pinned Agent Route') },
            @{ Route = 'backups'; Text = @('Backups', 'Hot Backup', 'Recovery Surface') },
            @{ Route = 'projects'; Text = @('Projects', 'Project Memory', 'Agent Policy') },
            @{ Route = 'logs'; Text = @('Logs', 'Diagnostics', 'tray.log') },
            @{ Route = 'inbox'; Text = @('Memory inbox', 'Review queue', 'Contradictions') },
            @{ Route = 'import'; Text = @('Import', 'Source', 'Local path') },
            @{ Route = 'assistant'; Text = @('Assistant', 'Agent Chat', 'The Assistant will use your Solo memory') }
        )
        foreach ($page in $pages) {
            $routeUrl = Join-DesktopRouteUrl -DesktopUrl $DesktopUrl -Route $page.Route
            $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
            [System.IO.File]::WriteAllText($routeFile, $routeUrl, $utf8NoBom)
            [void](Wait-ForDesktopSmokeReport `
                    -ReportFile $reportFile `
                    -ExpectedHash "#$($page.Route)" `
                    -RequiredText $page.Text `
                    -TimeoutSeconds $RouteTimeoutSeconds)
            Write-Host "desktop route ok: #$($page.Route)"
        }
        Write-Host "desktop click smoke ok: pid=$($desktop.Id), routes=$($pages.Count), report=$reportFile"
    }
    finally {
        Stop-SmokeProcess -Process $desktop
    }
}

function Test-DocumentImportSourceRouting {
    param(
        [string]$BaseUrl,
        [string]$DataDir
    )

    $importDir = Join-Path $DataDir 'import-source-routing'
    New-Item -ItemType Directory -Force -Path $importDir | Out-Null
    Set-Content -LiteralPath (Join-Path $importDir 'a.md') -Value "# Markdown smoke`nbody" -Encoding UTF8
    Set-Content -LiteralPath (Join-Path $importDir 'b.txt') -Value 'plain text smoke' -Encoding UTF8
    Set-Content -LiteralPath (Join-Path $importDir 'skip.json') -Value '{"skip":true}' -Encoding UTF8

    $dryRun = Invoke-Json -Method Post -Uri "$BaseUrl/memory/documents/import" -Body @{
        path      = $importDir
        source    = 'markdown_text'
        dry_run   = $true
        max_files = 10
    }
    if ([int]$dryRun.total_files -ne 2) {
        throw "markdown_text dry-run returned $($dryRun.total_files) files; expected 2"
    }
    if ($dryRun.source -ne 'markdown_text' -or $dryRun.source_label -ne 'Markdown/Text') {
        throw "markdown_text dry-run returned unexpected source metadata: source=$($dryRun.source), label=$($dryRun.source_label)"
    }
    $dryRunPaths = @($dryRun.files | ForEach-Object { [string]$_.path })
    if (!($dryRunPaths | Where-Object { $_.EndsWith('a.md') })) {
        throw "markdown_text dry-run did not include a.md: $($dryRunPaths -join ', ')"
    }
    if (!($dryRunPaths | Where-Object { $_.EndsWith('b.txt') })) {
        throw "markdown_text dry-run did not include b.txt: $($dryRunPaths -join ', ')"
    }
    if ($dryRunPaths | Where-Object { $_.EndsWith('skip.json') }) {
        throw "markdown_text dry-run included skip.json: $($dryRunPaths -join ', ')"
    }

    $imported = Invoke-Json -Method Post -Uri "$BaseUrl/memory/documents/import" -Body @{
        path      = $importDir
        source    = 'markdown_text'
        dry_run   = $false
        max_files = 10
    }
    if ([int]$imported.failed -ne 0) {
        throw "markdown_text import reported $($imported.failed) failed file(s)"
    }
    $stored = [int]$imported.imported + [int]$imported.deduped
    if ($stored -ne 2) {
        throw "markdown_text import stored $stored files; expected 2"
    }
    if ($imported.source -ne 'markdown_text' -or $imported.source_label -ne 'Markdown/Text') {
        throw "markdown_text import returned unexpected source metadata: source=$($imported.source), label=$($imported.source_label)"
    }
    if ([int]$imported.chunks_persisted -lt 1 -and [int]$imported.imported -gt 0) {
        throw 'markdown_text import reported imported files but no persisted chunks'
    }
    Write-Host "document import ok: markdown_text stored=$stored"
}

$root = (Resolve-Path -LiteralPath '.').Path
$smokeStamp = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$ownsDataDir = [string]::IsNullOrWhiteSpace($DataDir)
if ([string]::IsNullOrWhiteSpace($DataDir)) {
    $DataDir = Join-Path $root ".smoke\windows-installed-smoke-$smokeStamp"
}
$DataDir = Get-CanonicalPath -Path $DataDir
$defaultSoloDataDir = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.solo'
if (Test-PathIsSameOrChild -Path $DataDir -Root $defaultSoloDataDir) {
    throw "Refusing to run installed smoke in or below the real Solo data dir: $DataDir"
}

$sentinelPath = Join-Path $DataDir '.solo-installed-smoke-disposable.json'
$resetExistingTestDir = $false
if (Test-Path -LiteralPath $DataDir) {
    $dataDirItem = Get-Item -LiteralPath $DataDir -Force
    if (($dataDirItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Refusing a reparse-point DataDir because its deletion target is ambiguous: $DataDir"
    }
    if (Test-Path -LiteralPath (Join-Path $DataDir 'solo.lock')) {
        throw "Refusing to touch data dir with an active/stale Solo lock: $DataDir"
    }
    $existingEntries = @(Get-ChildItem -LiteralPath $DataDir -Force)
    if ($existingEntries.Count -gt 0) {
        if (!$AllowDestructiveExistingTestDataDir) {
            throw "DataDir must be empty and disposable. To reset a prior smoke-owned directory, pass -AllowDestructiveExistingTestDataDir: $DataDir"
        }
        if (!(Test-Path -LiteralPath $sentinelPath -PathType Leaf)) {
            throw "Destructive reset is allowed only for a directory carrying the installed-smoke sentinel; refusing: $DataDir"
        }
        $sentinelItem = Get-Item -LiteralPath $sentinelPath -Force
        if (($sentinelItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Refusing a reparse-point installed-smoke sentinel: $sentinelPath"
        }
        try {
            $existingSentinel = Get-Content -Raw -LiteralPath $sentinelPath | ConvertFrom-Json
        }
        catch {
            throw "Installed-smoke sentinel is not valid JSON; refusing destructive reset: $sentinelPath"
        }
        if ($existingSentinel.purpose -ne 'solo-windows-installed-smoke-disposable' -or
            [string]::IsNullOrWhiteSpace([string]$existingSentinel.data_dir) -or
            !(Test-SamePath -Left ([string]$existingSentinel.data_dir) -Right $DataDir)) {
            throw "Installed-smoke sentinel does not prove ownership of this DataDir; refusing destructive reset: $sentinelPath"
        }
        $resetExistingTestDir = $true
        $ownsDataDir = $true
    }
}

$solo = Join-Path $InstallDir 'solo.exe'
$tray = Join-Path $InstallDir 'solo-tray.exe'
if (!(Test-Path -LiteralPath $solo)) {
    throw "solo.exe not found at $solo"
}
if (!(Test-Path -LiteralPath $tray)) {
    throw "solo-tray.exe not found at $tray"
}

Write-Step "Installed binary versions"
Assert-InstalledBinaryVersion -Binary $solo -Label 'solo.exe'
Assert-InstalledBinaryVersion -Binary $tray -Label 'solo-tray.exe'

Write-Step "Reserve a non-conflicting loopback port"
$portReservation = Reserve-SmokePort -RequestedPort $Port
$Port = [int]$portReservation.LocalEndpoint.Port
Write-Host "reserved loopback port: $Port"

Write-Step "Initialize smoke data dir"
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
if ($resetExistingTestDir) {
    Get-ChildItem -LiteralPath $DataDir -Force | Remove-Item -Recurse -Force
}
$sentinel = [ordered]@{
    purpose    = 'solo-windows-installed-smoke-disposable'
    created_at = [DateTime]::UtcNow.ToString('o')
    data_dir   = $DataDir
    script_pid = $PID
} | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText($sentinelPath, $sentinel, [System.Text.UTF8Encoding]::new($false))
$oldPassphrase = $env:SOLO_PASSPHRASE
$env:SOLO_PASSPHRASE = $Passphrase
try {
    & $solo init --data-dir $DataDir | Out-Host
}
finally {
    if ($null -eq $oldPassphrase) {
        Remove-Item Env:\SOLO_PASSPHRASE -ErrorAction SilentlyContinue
    }
    else {
        $env:SOLO_PASSPHRASE = $oldPassphrase
    }
}

Write-Step "Start installed daemon on port $Port"
$baseUrl = "http://127.0.0.1:$Port"
$processInfo = [System.Diagnostics.ProcessStartInfo]::new()
$processInfo.FileName = $solo
$processInfo.UseShellExecute = $false
$processInfo.CreateNoWindow = $true
$processInfo.Arguments = (@('daemon', '--data-dir', $DataDir, '--http-port', [string]$Port) |
    ForEach-Object { Quote-ProcessArgument $_ }) -join ' '
$oldDaemonPassphrase = $env:SOLO_PASSPHRASE
$env:SOLO_PASSPHRASE = $Passphrase
try {
    $portReservation.Stop()
    $portReservation = $null
    $daemon = [System.Diagnostics.Process]::Start($processInfo)
    if ($null -eq $daemon) {
        throw 'Process.Start returned no installed daemon process'
    }
}
finally {
    if ($null -eq $oldDaemonPassphrase) {
        Remove-Item Env:\SOLO_PASSPHRASE -ErrorAction SilentlyContinue
    }
    else {
        $env:SOLO_PASSPHRASE = $oldDaemonPassphrase
    }
}

$restoreMemoryId = $null
$restoreBackupPath = $null
$restoreExpectedContent = $null
$documentLifecycle = $null
try {
    $status = Wait-ForStatus -BaseUrl $baseUrl -TimeoutSeconds $TimeoutSeconds -DaemonProcess $daemon
    if ([string]$status.build.version -ne '0.12.0') {
        throw "Connected daemon semantic version must be exactly 0.12.0; got '$($status.build.version)'"
    }
    if ([string]$status.version -ne [string]$status.build.version_with_build) {
        throw "Status version '$($status.version)' does not match build.version_with_build '$($status.build.version_with_build)'"
    }
    if ([int]$status.runtime.pid -ne $daemon.Id) {
        throw "Status endpoint belongs to PID $($status.runtime.pid), not spawned smoke daemon PID $($daemon.Id)"
    }
    $runtimeDataDir = Get-CanonicalPath -Path ([string]$status.runtime.data_dir)
    if (!(Test-SamePath -Left $runtimeDataDir -Right $DataDir)) {
        throw "Status endpoint data_dir '$runtimeDataDir' does not belong to disposable smoke dir '$DataDir'"
    }
    if (!(Test-Path -LiteralPath $sentinelPath -PathType Leaf)) {
        throw "Disposable smoke ownership sentinel disappeared before API mutation: $sentinelPath"
    }
    Start-Sleep -Seconds 1
    $daemon.Refresh()
    if ($daemon.HasExited) {
        throw "Installed daemon exited immediately after readiness with code $($daemon.ExitCode)"
    }
    Write-Host "status ownership ok: pid=$($daemon.Id), library=$($status.library.name), version=$($status.version), data_dir=$runtimeDataDir"

    Write-Step "Check packaged Desktop route"
    $desktop = Invoke-WebRequest -Uri "$baseUrl/desktop/" -UseBasicParsing
    if ($desktop.StatusCode -ne 200 -or [string]::IsNullOrWhiteSpace($desktop.Content)) {
        throw "/desktop/ returned an empty or non-200 response"
    }
    Write-Host "desktop route ok: $($desktop.Content.Length) bytes"

    Write-Step "Daemon hot backup endpoint"
    $backupPath = Join-Path $DataDir "solo-hot-backup-$smokeStamp-$([guid]::NewGuid().ToString('N').Substring(0, 8)).db"
    $backup = Invoke-Json -Method Post -Uri "$baseUrl/backup" -Body @{
        to    = $backupPath
        force = $false
    }
    if (!(Test-Path -LiteralPath $backupPath)) {
        throw "backup endpoint reported $($backup.path) but no file exists at $backupPath"
    }
    if ([string]$backup.path -ne $backupPath) {
        throw "backup endpoint returned path '$($backup.path)', expected '$backupPath'"
    }
    if ([int]$backup.elapsed_ms -lt 0) {
        throw "backup endpoint returned invalid elapsed_ms $($backup.elapsed_ms)"
    }
    Write-Host "backup ok: $($backup.path)"

    $backupForce = Invoke-Json -Method Post -Uri "$baseUrl/backup" -Body @{
        to    = $backupPath
        force = $true
    }
    if (!(Test-Path -LiteralPath $backupPath)) {
        throw "force backup reported $($backupForce.path) but no file exists at $backupPath"
    }
    if ([string]$backupForce.path -ne $backupPath) {
        throw "force backup returned path '$($backupForce.path)', expected '$backupPath'"
    }
    if ([int]$backupForce.elapsed_ms -lt 0) {
        throw "force backup returned invalid elapsed_ms $($backupForce.elapsed_ms)"
    }
    Write-Host "backup force ok: $($backupForce.path)"

    Write-Step "Document import source routing"
    Test-DocumentImportSourceRouting -BaseUrl $baseUrl -DataDir $DataDir

    Write-Step 'Staged document upload, provenance, and soft forget'
    $documentLifecycle = Test-StagedDocumentLifecycle -BaseUrl $baseUrl -DataDir $DataDir -Stamp $smokeStamp

    if ($DesktopClickSmoke) {
        Write-Step "Route installed Solo app window"
        Start-DesktopClickSmoke `
            -Tray $tray `
            -DesktopUrl "$baseUrl/desktop/" `
            -DataDir $DataDir `
            -WindowTimeoutSeconds $DesktopWindowTimeoutSeconds `
            -RouteTimeoutSeconds $DesktopClickSmokeTimeoutSeconds
    }
    elseif ($DesktopWindowSmoke) {
        Write-Step "Open installed Solo app window"
        Start-DesktopWindowSmoke `
            -Tray $tray `
            -DesktopUrl "$baseUrl/desktop/" `
            -TimeoutSeconds $DesktopWindowTimeoutSeconds
    }

    Write-Step "Write one smoke memory"
    $remember = Invoke-Json -Method Post -Uri "$baseUrl/memory" -Body @{
        content     = "Solo installed smoke memory $(Get-Date -Format o)"
        source_type = 'smoke.windows_installed'
        salience    = 0.5
    }
    Write-Host "memory ok: $($remember.memory_id)"

    Write-Step 'Correct the disposable memory and verify the persisted response'
    $restoreExpectedContent = "Solo installed smoke memory corrected at $(Get-Date -Format o)"
    $corrected = Invoke-Json -Method Patch -Uri "$baseUrl/memory/$($remember.memory_id)" -Body @{
        content = $restoreExpectedContent
    }
    if ($corrected.memory_id -ne $remember.memory_id -or $corrected.content -ne $restoreExpectedContent) {
        throw 'memory correction did not return the corrected content for the same memory ID'
    }
    Write-Host "memory correction ok: $($remember.memory_id)"

    Write-Step "Memory Inbox review endpoint"
    $inbox = Invoke-Json -Method Get -Uri "$baseUrl/v1/inbox?limit=10"
    $inboxItem = @($inbox.items | Where-Object { $_.memory_id -eq $remember.memory_id })[0]
    if ($null -eq $inboxItem) {
        throw "Memory Inbox did not include smoke memory $($remember.memory_id)"
    }
    [void](Invoke-Json -Method Post -Uri "$baseUrl/v1/inbox/$($remember.memory_id)/review" -Body @{
        state = 'approved'
        note  = 'installed smoke'
    })
    $reviewedInbox = Invoke-Json -Method Get -Uri "$baseUrl/v1/inbox?limit=10"
    $reviewedItem = @($reviewedInbox.items | Where-Object { $_.memory_id -eq $remember.memory_id })[0]
    if ($null -eq $reviewedItem -or $reviewedItem.review_state -ne 'approved') {
        throw "Memory Inbox review state did not persist for $($remember.memory_id)"
    }
    Write-Host "inbox review ok: $($remember.memory_id) -> $($reviewedItem.review_state)"

    Write-Step "Project memory JSON endpoints"
    $project = @{
        name = 'Solo Smoke'
        id   = 'solo-smoke'
        root = $root
        tags = @('smoke', 'windows')
    }
    $policy = Invoke-Json -Method Post -Uri "$baseUrl/v1/project/policy" -Body @{
        project = $project
        client  = 'codex'
    }
    if ($policy.command -ne 'project policy' -or $policy.client -ne 'codex' -or
        $policy.policy -notlike '*Project id: solo-smoke*') {
        throw "Project policy endpoint returned an unexpected response"
    }
    $projectFacts = Invoke-Json -Method Post -Uri "$baseUrl/v1/project/facts" -Body @{
        project = $project
        subject = 'Solo Smoke'
        limit   = 3
    }
    if ($projectFacts.command -ne 'project facts' -or $projectFacts.project.id -ne 'solo-smoke') {
        throw "Project facts endpoint returned an unexpected response"
    }
    $projectDecision = Invoke-Json -Method Post -Uri "$baseUrl/v1/project/decisions" -Body @{
        project  = $project
        decision = 'Installed smoke uses daemon project endpoints.'
    }
    if ($projectDecision.command -ne 'project decisions' -or $projectDecision.action -ne 'add' -or
        $projectDecision.source_type -ne 'project_decision' -or
        $projectDecision.source_id -notlike 'project:solo-smoke:decision:*') {
        throw "Project decision add endpoint returned an unexpected response"
    }
    $projectDecisionSearch = Invoke-Json -Method Post -Uri "$baseUrl/v1/project/decisions/search" -Body @{
        project = $project
        query   = 'daemon project endpoints'
        limit   = 5
    }
    $projectDecisionHit = @($projectDecisionSearch.hits |
        Where-Object { $_.memory_id -eq $projectDecision.memory_id })[0]
    if ($null -eq $projectDecisionHit) {
        throw "Project decision search endpoint did not find the smoke decision"
    }
    Write-Host "project memory ok: $($projectDecision.memory_id)"

    Write-Step "MCP initialize"
    $initBody = @{
        jsonrpc = '2.0'
        id      = 1
        method  = 'initialize'
        params  = @{
            protocolVersion = '2025-03-26'
            capabilities    = @{}
            clientInfo      = @{
                name    = 'solo-windows-installed-smoke'
                version = '0.0.0'
            }
        }
    }
    $initResponse = Invoke-WebRequest -Method Post -Uri "$baseUrl/mcp" `
        -ContentType 'application/json' -Body (ConvertTo-JsonBody $initBody) `
        -UseBasicParsing
    $sessionId = Header-Value -Headers $initResponse.Headers -Name 'Mcp-Session-Id'
    if ([string]::IsNullOrWhiteSpace($sessionId)) {
        throw 'MCP initialize did not return Mcp-Session-Id'
    }

    $mcpHeaders = @{ 'Mcp-Session-Id' = $sessionId }
    [void](Invoke-Json -Method Post -Uri "$baseUrl/mcp" -Headers $mcpHeaders -Body @{
        jsonrpc = '2.0'
        method  = 'notifications/initialized'
        params  = @{}
    })

    Write-Step "MCP tools/list"
    $tools = Invoke-Json -Method Post -Uri "$baseUrl/mcp" -Headers $mcpHeaders -Body @{
        jsonrpc = '2.0'
        id      = 2
        method  = 'tools/list'
    }
    $toolNames = @($tools.result.tools | ForEach-Object { $_.name })
    if ($toolNames.Count -ne $ExpectedToolCount) {
        throw "MCP tools/list returned $($toolNames.Count) tools; expected $ExpectedToolCount"
    }
    if (!($toolNames -contains 'memory_context')) {
        throw "MCP tools/list did not include memory_context"
    }
    if (!($toolNames -contains 'memory_inbox')) {
        throw "MCP tools/list did not include memory_inbox"
    }
    if (!($toolNames -contains 'memory_review')) {
        throw "MCP tools/list did not include memory_review"
    }
    Write-Host "tools ok: $($toolNames.Count) tools"

    Write-Step "Setup-client Doctor tools/list"
    $doctorOutput = & $solo setup-client doctor codex --scope user --url "$baseUrl/mcp" --format json
    if ($LASTEXITCODE -ne 0) {
        throw "setup-client doctor exited with code $LASTEXITCODE"
    }
    $doctor = ($doctorOutput -join "`n") | ConvertFrom-Json
    if ($doctor.mcp_endpoint.status -ne 'reachable') {
        throw "setup-client doctor endpoint was $($doctor.mcp_endpoint.status): $($doctor.mcp_endpoint.detail)"
    }
    if ($null -eq $doctor.mcp_endpoint.tools) {
        throw "setup-client doctor did not report MCP tools"
    }
    if ([int]$doctor.mcp_endpoint.tools.tool_count -ne $ExpectedToolCount) {
        throw "setup-client doctor reported $($doctor.mcp_endpoint.tools.tool_count) tools; expected $ExpectedToolCount"
    }
    if (@($doctor.mcp_endpoint.tools.missing_required_tools).Count -ne 0) {
        throw "setup-client doctor reported missing tools: $($doctor.mcp_endpoint.tools.missing_required_tools -join ', ')"
    }
    Write-Host "doctor tools ok: $($doctor.mcp_endpoint.tools.tool_count) tools"


    Write-Step "MCP tools/call memory_review + memory_inbox"
    [void](Invoke-Json -Method Post -Uri "$baseUrl/mcp" -Headers $mcpHeaders -Body @{
        jsonrpc = '2.0'
        id      = 3
        method  = 'tools/call'
        params  = @{
            name      = 'memory_review'
            arguments = @{
                memory_id = $remember.memory_id
                state     = 'dismissed'
                note      = 'installed mcp smoke'
            }
        }
    })
    $mcpInbox = Invoke-Json -Method Post -Uri "$baseUrl/mcp" -Headers $mcpHeaders -Body @{
        jsonrpc = '2.0'
        id      = 4
        method  = 'tools/call'
        params  = @{
            name      = 'memory_inbox'
            arguments = @{
                limit = 10
            }
        }
    }
    $mcpInboxText = @($mcpInbox.result.content)[0].text
    $mcpInboxJson = $mcpInboxText | ConvertFrom-Json
    $mcpInboxItem = @($mcpInboxJson.items | Where-Object { $_.memory_id -eq $remember.memory_id })[0]
    if ($null -eq $mcpInboxItem -or $mcpInboxItem.review_state -ne 'dismissed') {
        throw "MCP memory_review/memory_inbox round-trip did not persist dismissed state"
    }
    Write-Host "mcp inbox review ok: $($remember.memory_id) -> $($mcpInboxItem.review_state)"

    Write-Step "MCP tools/call memory_context"
    $context = Invoke-Json -Method Post -Uri "$baseUrl/mcp" -Headers $mcpHeaders -Body @{
        jsonrpc = '2.0'
        id      = 5
        method  = 'tools/call'
        params  = @{
            name      = 'memory_context'
            arguments = @{
                query   = 'installed smoke memory'
                subject = 'Solo'
                limit   = 3
            }
        }
    }
    if ($context.result.isError -eq $true -or @($context.result.content).Count -lt 1) {
        throw 'memory_context returned an error or empty content'
    }
    Write-Host 'memory_context ok'

    if (!$SkipRestoreDrill) {
        Write-Step 'Capture final encrypted backup for restore drill'
        $restoreMemoryId = [string]$remember.memory_id
        $restoreBackupPath = Join-Path $DataDir "solo-restore-drill-$smokeStamp-$([guid]::NewGuid().ToString('N').Substring(0, 8)).db"
        $restoreBackup = Invoke-Json -Method Post -Uri "$baseUrl/backup" -Body @{
            to    = $restoreBackupPath
            force = $false
        }
        if (!(Test-Path -LiteralPath $restoreBackupPath -PathType Leaf)) {
            throw "restore-drill backup was not created at $restoreBackupPath"
        }
        if ([string]$restoreBackup.path -ne $restoreBackupPath) {
            throw "restore-drill backup returned path '$($restoreBackup.path)', expected '$restoreBackupPath'"
        }
        Write-Host "restore-drill backup ok: $restoreBackupPath"

        if ($null -eq $documentLifecycle -or [string]::IsNullOrWhiteSpace($documentLifecycle.AssetId)) {
            throw 'document lifecycle did not retain an asset for deletion/restore proof'
        }
        Write-Step 'Delete retained source bytes after the backup snapshot'
        $assetForgetResponse = Invoke-Json -Method Post -Uri "$baseUrl/mcp" -Headers $mcpHeaders -Body @{
            jsonrpc = '2.0'
            id      = 6
            method  = 'tools/call'
            params  = @{
                name      = 'memory_forget_asset'
                arguments = @{
                    asset_id = $documentLifecycle.AssetId
                }
            }
        }
        if ($assetForgetResponse.result.isError -eq $true -or @($assetForgetResponse.result.content).Count -lt 1) {
            throw 'memory_forget_asset returned an error or empty content'
        }
        $assetForget = ([string]$assetForgetResponse.result.content[0].text) | ConvertFrom-Json
        if ($assetForget.asset_id -ne $documentLifecycle.AssetId -or $assetForget.blob_deleted -ne $true) {
            throw 'memory_forget_asset did not report deletion of the retained source blob'
        }
        if (Test-Path -LiteralPath $documentLifecycle.AssetBlobPath) {
            throw "retained source blob still exists after deletion: $($documentLifecycle.AssetBlobPath)"
        }
        Write-Host "asset deletion ok: $($documentLifecycle.AssetId)"
    }

    Write-Step 'Installed Windows online smoke passed'
}
finally {
    if ($null -ne $portReservation) {
        $portReservation.Stop()
    }
    if ($null -ne $daemon -and !$daemon.HasExited) {
        $daemon.Kill()
        [void]$daemon.WaitForExit(5000)
    }
}

if (!$SkipRestoreDrill) {
    if ([string]::IsNullOrWhiteSpace($restoreMemoryId) -or
        [string]::IsNullOrWhiteSpace($restoreBackupPath)) {
        throw 'restore drill did not capture a memory ID and backup path'
    }

    Write-Step 'Prove installed backup restore after a disposable mutation'
    [void](Invoke-SoloWithPassphrase -Solo $solo -Passphrase $Passphrase -Arguments @(
        'forget', $restoreMemoryId,
        '--reason', 'installed-restore-drill',
        '--data-dir', $DataDir
    ))
    $forgotten = Invoke-SoloWithPassphrase -Solo $solo -Passphrase $Passphrase -Arguments @(
        'inspect', $restoreMemoryId,
        '--data-dir', $DataDir
    )
    if (($forgotten -join "`n") -notmatch '(?m)^status\s*:\s*forgotten\s*$') {
        throw "restore drill could not prove the disposable memory was forgotten:`n$($forgotten -join [Environment]::NewLine)"
    }

    [void](Invoke-SoloWithPassphrase -Solo $solo -Passphrase $Passphrase -Arguments @(
        'restore',
        '--from', $restoreBackupPath,
        '--data-dir', $DataDir,
        '--confirm'
    ))
    $restored = Invoke-SoloWithPassphrase -Solo $solo -Passphrase $Passphrase -Arguments @(
        'inspect', $restoreMemoryId,
        '--data-dir', $DataDir
    )
    if (($restored -join "`n") -notmatch '(?m)^status\s*:\s*active\s*$') {
        throw "restored memory is not active:`n$($restored -join [Environment]::NewLine)"
    }
    if (($restored -join "`n") -notlike "*$restoreExpectedContent*") {
        throw 'restored memory did not retain the corrected content from the backup snapshot'
    }
    if (!(Test-Path -LiteralPath $documentLifecycle.AssetBlobPath -PathType Leaf)) {
        throw "backup restore did not recover retained source bytes: $($documentLifecycle.AssetBlobPath)"
    }
    Write-Host "backup restore ok: memory $restoreMemoryId returned active"
}

Write-Step 'Installed Windows smoke passed'
