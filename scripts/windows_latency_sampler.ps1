param(
    [int]$DurationSeconds = 600,
    [int]$IntervalSeconds = 5,
    [int]$TopProcesses = 30,
    [string]$OutputDir = "",
    [string[]]$WatchNames = @(
        "solo",
        "solo-tray",
        "ollama",
        "node",
        "codex",
        "claude",
        "dwm",
        "explorer",
        "MsMpEng",
        "Discord",
        "NVIDIA Overlay",
        "steamwebhelper",
        "msedgewebview2"
    )
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Continue"

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptRoot
if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputDir = Join-Path $repoRoot ".smoke\windows-latency-$stamp"
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

$systemCsv = Join-Path $OutputDir "system.csv"
$processCsv = Join-Path $OutputDir "processes.csv"
$gpuCsv = Join-Path $OutputDir "gpu.csv"
$summaryTxt = Join-Path $OutputDir "summary.txt"

"timestamp,metric,value" | Set-Content -LiteralPath $systemCsv
"timestamp,process_name,pid,cpu_seconds,working_set_mb,private_mb,virtual_mb,handles,threads,path,reason" | Set-Content -LiteralPath $processCsv
"timestamp,pid,process_name,engine,value,path" | Set-Content -LiteralPath $gpuCsv

$systemCounters = @(
    "\Memory\Committed Bytes",
    "\Memory\Commit Limit",
    "\Memory\Pages/sec",
    "\Paging File(_Total)\% Usage",
    "\PhysicalDisk(_Total)\Avg. Disk Queue Length",
    "\PhysicalDisk(_Total)\% Disk Time",
    "\Processor(_Total)\% Processor Time"
)

function Escape-CsvValue {
    param([object]$Value)
    if ($null -eq $Value) {
        return ""
    }

    $text = [string]$Value
    if ($text.Contains('"') -or $text.Contains(",") -or $text.Contains("`n") -or $text.Contains("`r")) {
        return '"' + $text.Replace('"', '""') + '"'
    }

    return $text
}

function Write-CsvLine {
    param(
        [string]$Path,
        [object[]]$Values
    )

    $line = ($Values | ForEach-Object { Escape-CsvValue $_ }) -join ","
    Add-Content -LiteralPath $Path -Value $line
}

function Add-SystemSample {
    param([string]$Timestamp)

    try {
        $samples = Get-Counter $systemCounters -ErrorAction Stop
        foreach ($sample in $samples.CounterSamples) {
            Write-CsvLine $systemCsv @($Timestamp, $sample.Path, $sample.CookedValue)
        }
    } catch {
        Write-CsvLine $systemCsv @($Timestamp, "counter_error", $_.Exception.Message)
    }
}

function Add-ProcessSamples {
    param([string]$Timestamp)

    $processes = @(Get-Process -ErrorAction SilentlyContinue)
    $watched = @{}
    foreach ($name in $WatchNames) {
        if (-not [string]::IsNullOrWhiteSpace($name)) {
            $watched[$name.ToLowerInvariant()] = $true
        }
    }

    $selected = @{}
    foreach ($process in ($processes | Sort-Object PrivateMemorySize64 -Descending | Select-Object -First $TopProcesses)) {
        $selected[[int]$process.Id] = "top_private_memory"
    }
    foreach ($process in ($processes | Sort-Object HandleCount -Descending | Select-Object -First $TopProcesses)) {
        if ($selected.ContainsKey([int]$process.Id)) {
            $selected[[int]$process.Id] += "+top_handles"
        } else {
            $selected[[int]$process.Id] = "top_handles"
        }
    }
    foreach ($process in ($processes | Sort-Object { $_.Threads.Count } -Descending | Select-Object -First $TopProcesses)) {
        if ($selected.ContainsKey([int]$process.Id)) {
            $selected[[int]$process.Id] += "+top_threads"
        } else {
            $selected[[int]$process.Id] = "top_threads"
        }
    }
    foreach ($process in $processes) {
        if ($watched.ContainsKey($process.ProcessName.ToLowerInvariant())) {
            if ($selected.ContainsKey([int]$process.Id)) {
                $selected[[int]$process.Id] += "+watched"
            } else {
                $selected[[int]$process.Id] = "watched"
            }
        }
    }

    foreach ($process in ($processes | Where-Object { $selected.ContainsKey([int]$_.Id) } | Sort-Object ProcessName, Id)) {
        $path = ""
        try {
            $path = $process.Path
        } catch {
            $path = ""
        }

        Write-CsvLine $processCsv @(
            $Timestamp,
            $process.ProcessName,
            $process.Id,
            $process.CPU,
            [math]::Round($process.WorkingSet64 / 1MB, 2),
            [math]::Round($process.PrivateMemorySize64 / 1MB, 2),
            [math]::Round($process.VirtualMemorySize64 / 1MB, 2),
            $process.HandleCount,
            $process.Threads.Count,
            $path,
            $selected[[int]$process.Id]
        )
    }
}

function Add-GpuSamples {
    param([string]$Timestamp)

    try {
        $samples = Get-Counter "\GPU Engine(*)\Utilization Percentage" -ErrorAction Stop
        $processById = @{}
        foreach ($process in (Get-Process -ErrorAction SilentlyContinue)) {
            $processById[[int]$process.Id] = $process.ProcessName
        }

        foreach ($sample in ($samples.CounterSamples | Where-Object { $_.CookedValue -gt 0.1 })) {
            $pid = ""
            $processName = ""
            $engine = ""

            if ($sample.Path -match "pid_(\d+)") {
                $pid = [int]$Matches[1]
                if ($processById.ContainsKey($pid)) {
                    $processName = $processById[$pid]
                }
            }
            if ($sample.Path -match "engtype_([^)\s]+)") {
                $engine = $Matches[1]
            }

            Write-CsvLine $gpuCsv @(
                $Timestamp,
                $pid,
                $processName,
                $engine,
                [math]::Round($sample.CookedValue, 2),
                $sample.Path
            )
        }
    } catch {
        Write-CsvLine $gpuCsv @($Timestamp, "", "", "counter_error", "", $_.Exception.Message)
    }
}

$startedAt = Get-Date
$endAt = $startedAt.AddSeconds($DurationSeconds)

@(
    "Windows latency sampler",
    "Started: $($startedAt.ToString("o"))",
    "DurationSeconds: $DurationSeconds",
    "IntervalSeconds: $IntervalSeconds",
    "TopProcesses: $TopProcesses",
    "OutputDir: $OutputDir",
    "WatchNames: $($WatchNames -join ', ')"
) | Set-Content -LiteralPath $summaryTxt

Write-Host "Writing latency samples to $OutputDir"
Write-Host "Press Ctrl+C to stop early."

do {
    $timestamp = (Get-Date).ToString("o")
    Add-SystemSample -Timestamp $timestamp
    Add-ProcessSamples -Timestamp $timestamp
    Add-GpuSamples -Timestamp $timestamp

    if ((Get-Date) -lt $endAt) {
        Start-Sleep -Seconds $IntervalSeconds
    }
} while ((Get-Date) -lt $endAt)

"Completed: $((Get-Date).ToString("o"))" | Add-Content -LiteralPath $summaryTxt
Write-Host "Done. Output: $OutputDir"
