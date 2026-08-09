#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Solo'),
    [string]$DataDir = '',
    [int]$Port = 17877,
    [string]$Passphrase = 'solo-mcp-client-smoke-passphrase',
    [int]$ExpectedToolCount = 39,
    [int]$TimeoutSeconds = 30,
    [switch]$UseExistingDaemon,
    [switch]$RequireCodexClientLoad,
    [switch]$RequireClaudeCodeClientLoad,
    [switch]$ClaudeCodeProjectLoadSmoke,
    [switch]$RequireClaudeCodeProjectLoad,
    [string]$ReportPath = ''
)

$ErrorActionPreference = 'Stop'
$SmokeRemovedEnv = @(
    'ANTHROPIC_API_KEY',
    'OPENAI_API_KEY',
    'OPENAI_BASE_URL',
    'OPENAI_MODEL',
    'SOLO_BGE_M3_DIR',
    'SOLO_EMBEDDER'
)

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message"
}

function ConvertTo-JsonBody {
    param([hashtable]$Value)
    return ($Value | ConvertTo-Json -Depth 16 -Compress)
}

function Write-Utf8NoBom {
    param(
        [string]$Path,
        [string]$Value
    )

    $parent = Split-Path -Parent $Path
    if (![string]::IsNullOrWhiteSpace($parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $Value, $utf8NoBom)
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

function Header-Value {
    param($Headers, [string]$Name)
    $value = $Headers[$Name]
    if ($value -is [array]) {
        return $value[0]
    }
    return $value
}

function New-McpHeaders {
    param([string]$SessionId = '')

    $headers = @{}
    if (![string]::IsNullOrWhiteSpace($SessionId)) {
        $headers['Mcp-Session-Id'] = $SessionId
    }
    return $headers
}

function Quote-ProcessArgument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') {
        return $Value
    }
    return '"' + ($Value -replace '"', '\"') + '"'
}

function Wait-ForStatus {
    param(
        [string]$BaseUrl,
        [int]$TimeoutSeconds
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $lastError = $null
    while ([DateTime]::UtcNow -lt $deadline) {
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

function Stop-SmokeProcess {
    param([System.Diagnostics.Process]$Process)
    if ($null -eq $Process -or $Process.HasExited) {
        return
    }
    $Process.Kill()
    [void]$Process.WaitForExit(5000)
}

function Set-SmokeProcessEnvironment {
    param(
        [hashtable]$Set = @{},
        [string[]]$Remove = @()
    )

    $snapshot = @{}
    $names = @{}
    foreach ($name in $Remove) {
        $names[[string]$name] = $true
    }
    foreach ($name in $Set.Keys) {
        $names[[string]$name] = $true
    }
    foreach ($name in $names.Keys) {
        $snapshot[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    }
    foreach ($name in $Remove) {
        [Environment]::SetEnvironmentVariable($name, $null, 'Process')
    }
    foreach ($name in $Set.Keys) {
        [Environment]::SetEnvironmentVariable($name, [string]$Set[$name], 'Process')
    }
    return $snapshot
}

function Restore-SmokeProcessEnvironment {
    param([hashtable]$Snapshot)

    foreach ($name in $Snapshot.Keys) {
        if ($null -eq $Snapshot[$name]) {
            [Environment]::SetEnvironmentVariable($name, $null, 'Process')
        }
        else {
            [Environment]::SetEnvironmentVariable($name, [string]$Snapshot[$name], 'Process')
        }
    }
}

function Invoke-ProcessCapture {
    param(
        [string]$FileName,
        [string[]]$Arguments,
        [int]$TimeoutSeconds = 15,
        [string]$WorkingDirectory = ''
    )

    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $FileName
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.Arguments = ($Arguments | ForEach-Object { Quote-ProcessArgument $_ }) -join ' '
    if (![string]::IsNullOrWhiteSpace($WorkingDirectory)) {
        $info.WorkingDirectory = $WorkingDirectory
    }

    try {
        $process = [System.Diagnostics.Process]::Start($info)
    }
    catch {
        return [pscustomobject]@{
            started  = $false
            timedOut = $false
            exitCode = $null
            stdout   = ''
            stderr   = $_.Exception.Message
        }
    }

    $finished = $process.WaitForExit($TimeoutSeconds * 1000)
    if (!$finished) {
        Stop-SmokeProcess -Process $process
        return [pscustomobject]@{
            started  = $true
            timedOut = $true
            exitCode = $null
            stdout   = $process.StandardOutput.ReadToEnd()
            stderr   = $process.StandardError.ReadToEnd()
        }
    }

    return [pscustomobject]@{
        started  = $true
        timedOut = $false
        exitCode = $process.ExitCode
        stdout   = $process.StandardOutput.ReadToEnd()
        stderr   = $process.StandardError.ReadToEnd()
    }
}

function Invoke-SoloInit {
    param(
        [string]$Solo,
        [string]$DataDir,
        [string]$Passphrase,
        [int]$TimeoutSeconds
    )

    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Solo
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.Arguments = (@('init', '--data-dir', $DataDir) |
        ForEach-Object { Quote-ProcessArgument $_ }) -join ' '

    $envSnapshot = Set-SmokeProcessEnvironment `
        -Set @{ SOLO_PASSPHRASE = $Passphrase } `
        -Remove $SmokeRemovedEnv
    try {
        $process = [System.Diagnostics.Process]::Start($info)
    }
    finally {
        Restore-SmokeProcessEnvironment -Snapshot $envSnapshot
    }
    $process.StandardInput.WriteLine('')
    $process.StandardInput.Close()

    $finished = $process.WaitForExit($TimeoutSeconds * 1000)
    if (!$finished) {
        Stop-SmokeProcess -Process $process
    }
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    if (!$finished) {
        throw "solo init timed out after $TimeoutSeconds seconds"
    }
    if (![string]::IsNullOrWhiteSpace($stdout)) {
        Write-Host $stdout.TrimEnd()
    }
    if (![string]::IsNullOrWhiteSpace($stderr)) {
        Write-Host $stderr.TrimEnd()
    }
    if ($process.ExitCode -ne 0) {
        throw "solo init exited with code $($process.ExitCode)"
    }
}

function Start-SoloDaemon {
    param(
        [string]$Solo,
        [string]$DataDir,
        [int]$Port,
        [string]$Passphrase
    )

    $processInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $processInfo.FileName = $Solo
    $processInfo.UseShellExecute = $false
    $processInfo.CreateNoWindow = $true
    $processInfo.RedirectStandardInput = $true
    $processInfo.RedirectStandardOutput = $true
    $processInfo.RedirectStandardError = $true
    $processInfo.Arguments = (@('daemon', '--data-dir', $DataDir, '--http-port', [string]$Port) |
        ForEach-Object { Quote-ProcessArgument $_ }) -join ' '

    $envSnapshot = Set-SmokeProcessEnvironment `
        -Set @{ SOLO_PASSPHRASE_STDIN = '1' } `
        -Remove @($SmokeRemovedEnv + @('SOLO_PASSPHRASE'))
    try {
        $process = [System.Diagnostics.Process]::Start($processInfo)
    }
    finally {
        Restore-SmokeProcessEnvironment -Snapshot $envSnapshot
    }
    $process.StandardInput.WriteLine($Passphrase)
    $process.StandardInput.Flush()
    return $process
}

function Test-RawMcp {
    param(
        [string]$BaseUrl,
        [int]$ExpectedToolCount
    )

    $routeLabel = 'Community Memory Library'
    Write-Step "Raw MCP initialize ($routeLabel)"
    $initBody = @{
        jsonrpc = '2.0'
        id      = 1
        method  = 'initialize'
        params  = @{
            protocolVersion = '2025-03-26'
            capabilities    = @{}
            clientInfo      = @{
                name    = 'solo-windows-mcp-client-smoke'
                version = '0.0.0'
            }
        }
    }
    $initHeaders = New-McpHeaders
    $initResponse = Invoke-WebRequest -Method Post -Uri "$BaseUrl/mcp" `
        -Headers $initHeaders `
        -ContentType 'application/json' -Body (ConvertTo-JsonBody $initBody) `
        -UseBasicParsing
    $sessionId = Header-Value -Headers $initResponse.Headers -Name 'Mcp-Session-Id'
    if ([string]::IsNullOrWhiteSpace($sessionId)) {
        throw 'MCP initialize did not return Mcp-Session-Id'
    }

    $headers = New-McpHeaders -SessionId $sessionId
    [void](Invoke-Json -Method Post -Uri "$BaseUrl/mcp" -Headers $headers -Body @{
        jsonrpc = '2.0'
        method  = 'notifications/initialized'
        params  = @{}
    })

    Write-Step "Raw MCP tools/list ($routeLabel)"
    $tools = Invoke-Json -Method Post -Uri "$BaseUrl/mcp" -Headers $headers -Body @{
        jsonrpc = '2.0'
        id      = 2
        method  = 'tools/list'
    }
    $toolNames = @($tools.result.tools | ForEach-Object { $_.name })
    if ($toolNames.Count -ne $ExpectedToolCount) {
        throw "MCP tools/list returned $($toolNames.Count) tools; expected $ExpectedToolCount"
    }
    foreach ($required in @('memory_context', 'memory_inbox', 'memory_review')) {
        if (!($toolNames -contains $required)) {
            throw "MCP tools/list did not include $required"
        }
    }

    Write-Step "Raw MCP tools/call memory_context ($routeLabel)"
    $context = Invoke-Json -Method Post -Uri "$BaseUrl/mcp" -Headers $headers -Body @{
        jsonrpc = '2.0'
        id      = 3
        method  = 'tools/call'
        params  = @{
            name      = 'memory_context'
            arguments = @{
                query   = 'solo mcp client smoke'
                subject = 'Solo'
                limit   = 3
            }
        }
    }
    if ($context.result.isError -eq $true -or @($context.result.content).Count -lt 1) {
        throw 'memory_context returned an error or empty content'
    }

    Write-Host "raw mcp ok: route=$routeLabel, session=$sessionId, tools=$($toolNames.Count)"
    return [pscustomobject]@{
        kind       = 'raw_mcp'
        status     = 'passed'
        route      = $routeLabel
        library    = 'Community Memory Library'
        session_id = $sessionId
        tool_count = $toolNames.Count
        detail     = 'initialize, tools/list, and memory_context passed'
    }
}

function Test-SetupClientDoctor {
    param(
        [string]$Solo,
        [string]$BaseUrl,
        [int]$ExpectedToolCount
    )

    $routeLabel = 'Community Memory Library'
    Write-Step "Setup-client doctor ($routeLabel)"
    $doctorArgs = @('setup-client', 'doctor', '--url', "$BaseUrl/mcp", '--format', 'json')
    $doctorOutput = & $Solo @doctorArgs
    if ($LASTEXITCODE -ne 0) {
        throw "setup-client doctor exited with code $LASTEXITCODE"
    }
    $doctor = ($doctorOutput -join "`n") | ConvertFrom-Json
    if ($doctor.mcp_endpoint.status -ne 'reachable') {
        throw "setup-client doctor endpoint was $($doctor.mcp_endpoint.status): $($doctor.mcp_endpoint.detail)"
    }
    if ($null -eq $doctor.mcp_endpoint.tools) {
        throw 'setup-client doctor did not report MCP tools'
    }
    if ([int]$doctor.mcp_endpoint.tools.tool_count -ne $ExpectedToolCount) {
        throw "setup-client doctor reported $($doctor.mcp_endpoint.tools.tool_count) tools; expected $ExpectedToolCount"
    }
    if (@($doctor.mcp_endpoint.tools.missing_required_tools).Count -ne 0) {
        throw "setup-client doctor reported missing tools: $($doctor.mcp_endpoint.tools.missing_required_tools -join ', ')"
    }

    foreach ($client in @($doctor.clients)) {
        $path = if ([string]::IsNullOrWhiteSpace($client.config_path)) { 'unavailable' } else { $client.config_path }
        Write-Host "doctor client: $($client.display_name) config=$($client.config_status) entry=$($client.solo_entry) path=$path"
        if ($client.config_status -eq 'invalid') {
            throw "setup-client doctor reported invalid $($client.display_name) config: $($client.detail)"
        }
    }
    Write-Host "doctor ok: route=$routeLabel, endpoint=$($doctor.mcp_endpoint.status), tools=$($doctor.mcp_endpoint.tools.tool_count)"
    return $doctor
}

function New-DoctorCheckResult {
    param($Doctor)

    $routeLabel = 'Community Memory Library'
    return [pscustomobject]@{
        kind       = 'setup_client_doctor'
        status     = 'passed'
        route      = $routeLabel
        library    = 'Community Memory Library'
        endpoint   = $Doctor.mcp_endpoint.status
        tool_count = [int]$Doctor.mcp_endpoint.tools.tool_count
        detail     = 'setup-client doctor endpoint and tools/list passed'
    }
}

function New-ClientSmokeResult {
    param(
        [string]$Client,
        [string]$Phase,
        [string]$Scope,
        $Result
    )

    return [pscustomobject]@{
        client  = $Client
        phase   = $Phase
        scope   = $Scope
        library = 'Community Memory Library'
        status  = $Result.status
        detail  = $Result.detail
    }
}

function Test-CodexClientLoad {
    param([switch]$Require)

    Write-Step 'Codex client load check'
    $command = Get-Command codex -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        $message = 'codex executable not found on PATH; run `codex mcp list` from a normal Codex terminal for the manual app-load smoke.'
        if ($Require) {
            throw $message
        }
        Write-Host "codex client: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }

    $result = Invoke-ProcessCapture -FileName $command.Source -Arguments @('mcp', 'list') -TimeoutSeconds 20
    $combined = (($result.stdout, $result.stderr) -join "`n").Trim()
    if ($result.timedOut) {
        $message = 'codex mcp list timed out'
        if ($Require) {
            throw $message
        }
        Write-Host "codex client: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }
    if (!$result.started -or $result.exitCode -ne 0) {
        $message = if ([string]::IsNullOrWhiteSpace($combined)) {
            "codex mcp list failed with exit code $($result.exitCode)"
        }
        else {
            "codex mcp list failed: $combined"
        }
        if ($Require) {
            throw $message
        }
        Write-Host "codex client: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }
    if ($combined -match '(?im)\bsolo\b') {
        Write-Host 'codex client: loaded (codex mcp list includes solo)'
        return [pscustomobject]@{ status = 'loaded'; detail = 'codex mcp list includes solo' }
    }

    $message = 'codex mcp list ran, but did not include solo'
    if ($Require) {
        throw $message
    }
    Write-Host "codex client: not ready ($message)"
    return [pscustomobject]@{ status = 'not_ready'; detail = $message }
}

function Test-ClaudeCodeClientLoad {
    param([switch]$Require)

    Write-Step 'Claude Code client load check'
    $command = Get-Command claude -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        $message = 'claude executable not found on PATH; run `claude mcp list` from a normal Claude Code terminal for the manual app-load smoke.'
        if ($Require) {
            throw $message
        }
        Write-Host "claude-code client: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }

    $result = Invoke-ProcessCapture -FileName $command.Source -Arguments @('mcp', 'list') -TimeoutSeconds 20
    $combined = (($result.stdout, $result.stderr) -join "`n").Trim()
    if ($result.timedOut) {
        $message = 'claude mcp list timed out'
        if ($Require) {
            throw $message
        }
        Write-Host "claude-code client: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }
    if (!$result.started -or $result.exitCode -ne 0) {
        $message = if ([string]::IsNullOrWhiteSpace($combined)) {
            "claude mcp list failed with exit code $($result.exitCode)"
        }
        else {
            "claude mcp list failed: $combined"
        }
        if ($Require) {
            throw $message
        }
        Write-Host "claude-code client: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }
    if ($combined -match '(?im)\bsolo\b') {
        Write-Host 'claude-code client: loaded (claude mcp list includes solo)'
        return [pscustomobject]@{ status = 'loaded'; detail = 'claude mcp list includes solo' }
    }

    $message = 'claude mcp list ran, but did not include solo'
    if ($Require) {
        throw $message
    }
    Write-Host "claude-code client: not ready ($message)"
    return [pscustomobject]@{ status = 'not_ready'; detail = $message }
}

function Test-ClaudeCodeProjectLoad {
    param(
        [string]$BaseUrl,
        [string]$ProjectDir,
        [switch]$Require
    )

    Write-Step 'Claude Code project-scope load smoke'
    $command = Get-Command claude -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        $message = 'claude executable not found on PATH'
        if ($Require) {
            throw $message
        }
        Write-Host "claude-code project smoke: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }

    New-Item -ItemType Directory -Force -Path $ProjectDir | Out-Null
    $addArgs = @(
        'mcp',
        'add',
        '--transport',
        'http',
        '--scope',
        'project',
        'solo',
        "$BaseUrl/mcp"
    )
    $add = Invoke-ProcessCapture -FileName $command.Source -Arguments $addArgs -TimeoutSeconds 20 -WorkingDirectory $ProjectDir
    $addOutput = (($add.stdout, $add.stderr) -join "`n").Trim()
    if ($add.timedOut -or !$add.started -or $add.exitCode -ne 0) {
        $message = if ($add.timedOut) {
            'claude mcp add --scope project timed out'
        }
        elseif ([string]::IsNullOrWhiteSpace($addOutput)) {
            "claude mcp add --scope project failed with exit code $($add.exitCode)"
        }
        else {
            "claude mcp add --scope project failed: $addOutput"
        }
        if ($Require) {
            throw $message
        }
        Write-Host "claude-code project smoke: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }

    $list = Invoke-ProcessCapture -FileName $command.Source -Arguments @('mcp', 'list') -TimeoutSeconds 20 -WorkingDirectory $ProjectDir
    $listOutput = (($list.stdout, $list.stderr) -join "`n").Trim()
    if ($list.timedOut -or !$list.started -or $list.exitCode -ne 0 -or $listOutput -notmatch '(?im)\bsolo\b') {
        $message = if ($list.timedOut) {
            'claude mcp list timed out after adding project-scoped Solo'
        }
        elseif ([string]::IsNullOrWhiteSpace($listOutput)) {
            "claude mcp list did not report Solo after project add; exit code $($list.exitCode)"
        }
        else {
            "claude mcp list did not report Solo after project add: $listOutput"
        }
        if ($Require) {
            throw $message
        }
        Write-Host "claude-code project smoke: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }

    $get = Invoke-ProcessCapture -FileName $command.Source -Arguments @('mcp', 'get', 'solo') -TimeoutSeconds 20 -WorkingDirectory $ProjectDir
    $getOutput = (($get.stdout, $get.stderr) -join "`n").Trim()
    if ($get.timedOut -or !$get.started -or $get.exitCode -ne 0 -or $getOutput -notmatch [regex]::Escape("$BaseUrl/mcp")) {
        $message = if ($get.timedOut) {
            'claude mcp get solo timed out after project add'
        }
        elseif ([string]::IsNullOrWhiteSpace($getOutput)) {
            "claude mcp get solo did not confirm the endpoint URL; exit code $($get.exitCode)"
        }
        else {
            "claude mcp get solo did not confirm the endpoint URL: $getOutput"
        }
        if ($Require) {
            throw $message
        }
        Write-Host "claude-code project smoke: manual ($message)"
        return [pscustomobject]@{ status = 'manual'; detail = $message }
    }

    Write-Host "claude-code project smoke: loaded (project .mcp.json lists $BaseUrl/mcp)"
    return [pscustomobject]@{ status = 'loaded'; detail = "project .mcp.json lists $BaseUrl/mcp" }
}

function Write-AppClientBoundary {
    param($Doctor)

    Write-Step 'Claude Desktop and Cursor app-load boundary'
    $results = New-Object System.Collections.Generic.List[object]
    foreach ($clientName in @('claude-desktop', 'cursor')) {
        $client = @($Doctor.clients | Where-Object { $_.client -eq $clientName })[0]
        if ($null -eq $client) {
            $detail = 'doctor did not return a row'
            Write-Host "$clientName client: manual ($detail)"
            $results.Add([pscustomobject]@{
                    client  = $clientName
                    phase   = 'app_load'
                    scope   = 'user'
                    library = 'Community Memory Library'
                    status  = 'manual'
                    detail  = $detail
                }) | Out-Null
            continue
        }
        if ($client.config_status -eq 'ok' -and $client.solo_entry -eq 'installed') {
            $detail = 'config is installed; open/restart the app and run a read-only memory_context call'
            Write-Host "$clientName client: manual app smoke required ($detail)"
            $status = 'manual'
        }
        else {
            $detail = "config=$($client.config_status), entry=$($client.solo_entry); run setup-client apply or use Connected Tools repair"
            Write-Host "$clientName client: not ready ($detail)"
            $status = 'not_ready'
        }
        $results.Add([pscustomobject]@{
                client        = $clientName
                phase         = 'app_load'
                scope         = 'user'
                library       = 'Community Memory Library'
                status        = $status
                detail        = $detail
                config_status = $client.config_status
                solo_entry    = $client.solo_entry
                config_path   = $client.config_path
            }) | Out-Null
    }
    return $results.ToArray()
}

function Write-SmokeSummary {
    param(
        [object[]]$Checks,
        [object[]]$Clients,
        [string]$ReportPath,
        [hashtable]$Meta
    )

    Write-Step 'MCP client smoke summary'
    foreach ($check in $Checks) {
        Write-Host "check $($check.kind): status=$($check.status), route=$($check.route), tools=$($check.tool_count)"
    }
    foreach ($client in $Clients) {
        Write-Host "client $($client.client)/$($client.phase): status=$($client.status), scope=$($client.scope), library=$($client.library), detail=$($client.detail)"
    }

    $summary = [pscustomobject]@{
        generated_at = (Get-Date).ToUniversalTime().ToString('o')
        meta         = $Meta
        checks       = $Checks
        clients      = $Clients
    }

    if (![string]::IsNullOrWhiteSpace($ReportPath)) {
        $json = $summary | ConvertTo-Json -Depth 16
        Write-Utf8NoBom -Path $ReportPath -Value $json
        Write-Host "summary report: $ReportPath"
    }
    return $summary
}

$root = (Resolve-Path -LiteralPath '.').Path
$baseUrl = "http://127.0.0.1:$Port"
$solo = Join-Path $InstallDir 'solo.exe'
if (!(Test-Path -LiteralPath $solo)) {
    throw "solo.exe not found at $solo"
}

if ($UseExistingDaemon -and ![string]::IsNullOrWhiteSpace($DataDir)) {
    throw '-DataDir is only used when this script starts its own daemon'
}

Write-Step 'Solo binary version'
& $solo --version

$smokeStamp = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$daemon = $null
$checkResults = New-Object System.Collections.Generic.List[object]
$clientResults = New-Object System.Collections.Generic.List[object]
try {
    if ($UseExistingDaemon) {
        Write-Step "Use existing daemon at $baseUrl"
    }
    else {
        if ([string]::IsNullOrWhiteSpace($DataDir)) {
            $DataDir = Join-Path $root ".smoke\windows-mcp-client-smoke-$smokeStamp"
        }

        Write-Step "Initialize smoke data dir at $DataDir"
        New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
        Invoke-SoloInit `
            -Solo $solo `
            -DataDir $DataDir `
            -Passphrase $Passphrase `
            -TimeoutSeconds $TimeoutSeconds

        Write-Step "Start daemon on port $Port"
        $daemon = Start-SoloDaemon `
            -Solo $solo `
            -DataDir $DataDir `
            -Port $Port `
            -Passphrase $Passphrase
    }

    $status = Wait-ForStatus -BaseUrl $baseUrl -TimeoutSeconds $TimeoutSeconds
    Write-Host "status ok: library=$($status.library.name), version=$($status.version)"

    $checkResults.Add((Test-RawMcp -BaseUrl $baseUrl -ExpectedToolCount $ExpectedToolCount)) | Out-Null
    $doctor = Test-SetupClientDoctor -Solo $solo -BaseUrl $baseUrl -ExpectedToolCount $ExpectedToolCount
    $checkResults.Add((New-DoctorCheckResult -Doctor $doctor)) | Out-Null
    if ($ClaudeCodeProjectLoadSmoke -or $RequireClaudeCodeProjectLoad) {
        $claudeProjectDir = Join-Path $root ".smoke\claude-code-project-mcp-smoke-$smokeStamp"
        $claudeProjectResult = Test-ClaudeCodeProjectLoad `
                -BaseUrl $baseUrl `
                -ProjectDir $claudeProjectDir `
                -Require:$RequireClaudeCodeProjectLoad
        $clientResults.Add((New-ClientSmokeResult `
                    -Client 'claude-code' `
                    -Phase 'project_load' `
                    -Scope 'project' `
                    -Result $claudeProjectResult)) | Out-Null
    }
    $codexResult = Test-CodexClientLoad -Require:$RequireCodexClientLoad
    $clientResults.Add((New-ClientSmokeResult `
                -Client 'codex' `
                -Phase 'app_load' `
                -Scope 'user' `
                -Result $codexResult)) | Out-Null
    $claudeCodeResult = Test-ClaudeCodeClientLoad -Require:$RequireClaudeCodeClientLoad
    $clientResults.Add((New-ClientSmokeResult `
                -Client 'claude-code' `
                -Phase 'app_load' `
                -Scope 'user' `
                -Result $claudeCodeResult)) | Out-Null
    foreach ($clientBoundary in @(Write-AppClientBoundary -Doctor $doctor)) {
        $clientResults.Add($clientBoundary) | Out-Null
    }

    [void](Write-SmokeSummary `
            -Checks $checkResults.ToArray() `
            -Clients $clientResults.ToArray() `
            -ReportPath $ReportPath `
            -Meta @{
                base_url          = $baseUrl
                data_dir          = $DataDir
                expected_tools    = $ExpectedToolCount
                library_name      = $status.library.name
                use_existing      = [bool]$UseExistingDaemon
                install_dir       = $InstallDir
                solo_version_line = (& $solo --version)
            })

    Write-Step 'Windows MCP client smoke passed'
}
finally {
    Stop-SmokeProcess -Process $daemon
}
