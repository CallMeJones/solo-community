param(
    [string]$PrepareResponsePath = "",
    [string]$McpUrl = "",
    [string]$BearerToken = "",
    [string]$BaseUploadOrigin = "",
    [switch]$AttemptPatch,
    [switch]$LegacySample,
    [switch]$AllowKnownGap
)

$ErrorActionPreference = "Stop"

function Redact-Headers {
    param([hashtable]$Headers)
    $result = @{}
    foreach ($key in $Headers.Keys) {
        $value = [string]$Headers[$key]
        if ($key.ToLowerInvariant() -eq "authorization") {
            $result[$key] = "Bearer [redacted]"
        } else {
            $result[$key] = $value
        }
    }
    return $result
}

function Convert-ObjectToHashtable {
    param($Object)
    $result = @{}
    if ($null -eq $Object) {
        return $result
    }
    if ($Object -is [System.Collections.IDictionary]) {
        foreach ($key in $Object.Keys) {
            $result[[string]$key] = $Object[$key]
        }
        return $result
    }
    foreach ($property in $Object.PSObject.Properties) {
        $result[$property.Name] = $property.Value
    }
    return $result
}

function New-McpHeaders {
    $headers = @{
        "accept" = "application/json, text/event-stream"
        "content-type" = "application/json"
    }
    if ($BearerToken) {
        $headers["authorization"] = "Bearer $BearerToken"
    }
    return $headers
}

function Invoke-McpTool {
    param(
        [string]$ToolName,
        [hashtable]$Arguments
    )

    if (-not $McpUrl) {
        throw "McpUrl is required to call $ToolName"
    }

    $payload = @{
        jsonrpc = "2.0"
        id = "phase0-upload-repro"
        method = "tools/call"
        params = @{
            name = $ToolName
            arguments = $Arguments
        }
    } | ConvertTo-Json -Depth 12

    $response = Invoke-RestMethod -Method Post -Uri $McpUrl -Headers (New-McpHeaders) -Body $payload
    if ($response.error) {
        throw "MCP tool $ToolName failed: $($response.error | ConvertTo-Json -Compress -Depth 8)"
    }
    return $response
}

function Read-WebErrorBody {
    param($Response)

    if ($null -eq $Response) {
        return $null
    }

    try {
        if ($Response.Content) {
            return [string]$Response.Content
        }
        $stream = $Response.GetResponseStream()
        if ($null -eq $stream) {
            return $null
        }
        $reader = [System.IO.StreamReader]::new($stream)
        try {
            return $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
    } catch {
        return $null
    }
}

function Read-WebErrorStatus {
    param($Response)

    if ($null -eq $Response) {
        return $null
    }

    try {
        if ($Response.StatusCode -is [int]) {
            return [int]$Response.StatusCode
        }
        return [int]$Response.StatusCode
    } catch {
        return $null
    }
}

function Read-PrepareResponse {
    if ($PrepareResponsePath) {
        return Get-Content -Raw -LiteralPath $PrepareResponsePath | ConvertFrom-Json
    }

    if ($McpUrl) {
        $response = Invoke-McpTool "document_upload_prepare" @{
            filename = "phase0-upload-repro.md"
            mime_type = "text/markdown"
            size_bytes = 12
        }
        if ($response.result.structuredContent) {
            return $response.result.structuredContent
        }
        $text = $response.result.content |
            Where-Object { $_.type -eq "text" -and $_.text } |
            Select-Object -First 1 -ExpandProperty text
        if (-not $text) {
            throw "MCP response did not contain structuredContent or text JSON"
        }
        return $text | ConvertFrom-Json
    }

    $sample = @{
        upload_id = "019eff78-d587-77b9-afe5-d3c01430c4f4"
        upload_url = "/uploads/019eff78-d587-77b9-afe5-d3c01430c4f4"
        upload_method = "PATCH"
        upload_content_type = "application/octet-stream"
        upload_offset_header = "upload-offset"
        upload_length_header = "upload-length"
        upload_status_header = "x-solo-upload-status"
        upload_headers = @{
            "upload-offset" = "0"
            "content-type" = "application/octet-stream"
            "upload-length" = "12"
        }
        upload_auth = @{
            mode = "same_as_solo_http"
            required = "when the Solo HTTP API is configured with auth"
            header = "authorization"
            note = "Direct Solo HTTP uploads use the same Authorization bearer as the rest of the Solo API. Solo Relay may rewrite this response with a short-lived upload token."
        }
        protocol = "solo-resumable-v1"
        max_file_bytes = 104857600
        recommended_chunk_bytes = 8388608
        expires_at_ms = 1780000000000
        next_steps = @(
            "Send raw file bytes to upload_url with upload_method and upload_headers; do not base64-encode the file in MCP tool arguments.",
            "If interrupted, call document_upload_status and resume with upload-offset set to next_offset.",
            "After bytes_received equals size_bytes, call document_upload_commit with upload_id and optional sha256.",
            "Call memory_ingest_staged_document with the returned staged_uri, then validate with memory_search_docs or memory_inspect_document."
        )
    }

    if ($LegacySample) {
        return $sample
    }

    $sample["upload_path"] = $sample.upload_url
    $sample["route_kind"] = "direct_local"
    $sample["required_headers"] = $sample.upload_headers
    $sample["max_chunk_bytes"] = 8388608
    $sample["mcp_fallback"] = @{
        tool = "document_upload_chunk_base64"
        max_chunk_bytes = 524288
        max_file_bytes = 524288
        encoding = "base64"
        preferred = $false
        note = "Use only when the client cannot send raw HTTP PATCH bytes. Raw HTTP is preferred for document uploads."
    }
    $sample["commit_tool"] = "document_upload_commit"
    $sample["ingest_tool"] = "memory_ingest_staged_document"
    $sample["default_store_original_file"] = $false
    $sample["next_actions"] = @(
        @{
            action = "upload_bytes"
            transport = "raw_http"
            method = "PATCH"
            url_field = "upload_url"
            headers_field = "required_headers"
            when = "preferred"
        },
        @{
            action = "upload_bytes_base64"
            transport = "mcp_tool"
            tool = "document_upload_chunk_base64"
            when = "only_if_raw_http_unavailable_and_file_fits_mcp_fallback"
        },
        @{
            action = "commit"
            transport = "mcp_tool"
            tool = "document_upload_commit"
            when = "after_bytes_received_equals_size_bytes"
        },
        @{
            action = "ingest"
            transport = "mcp_tool"
            tool = "memory_ingest_staged_document"
            when = "after_commit_returns_staged_uri"
        }
    )
    $sample["next_steps"] = @(
        "Send raw file bytes to upload_url with upload_method and required_headers.",
        "If interrupted, call document_upload_status and resume with upload-offset set to next_offset.",
        "After bytes_received equals size_bytes, call document_upload_commit with upload_id and optional sha256.",
        "Call memory_ingest_staged_document with the returned staged_uri, then validate with memory_search_docs or memory_inspect_document."
    )
    return $sample
}

$prepare = Read-PrepareResponse
$prepareHash = Convert-ObjectToHashtable $prepare
$headers = Convert-ObjectToHashtable $prepare.upload_headers
if ($prepare.required_headers) {
    $headers = Convert-ObjectToHashtable $prepare.required_headers
}

$uploadUrl = [string]$prepare.upload_url
$isAbsoluteUrl = $uploadUrl -match "^https?://"
$resolvedUploadUrl = $uploadUrl
if (-not $isAbsoluteUrl -and $BaseUploadOrigin) {
    $resolvedUploadUrl = ([System.Uri]::new([System.Uri]::new($BaseUploadOrigin), $uploadUrl)).AbsoluteUri
}

$missingV2Fields = @()
foreach ($field in @(
    "upload_path",
    "route_kind",
    "required_headers",
    "max_chunk_bytes",
    "commit_tool",
    "ingest_tool",
    "default_store_original_file",
    "next_actions"
)) {
    if (-not $prepareHash.ContainsKey($field)) {
        $missingV2Fields += $field
    }
}

$failureClass = @()
if (-not $isAbsoluteUrl -and ([string]$prepare.route_kind) -ne "direct_local") {
    $failureClass += "relative_url"
}
if (-not $prepareHash.ContainsKey("required_headers")) {
    $failureClass += "headers_under_legacy_upload_headers"
}
if (-not $prepareHash.ContainsKey("route_kind")) {
    $failureClass += "missing_route_kind"
}
if (-not $prepareHash.ContainsKey("next_actions")) {
    $failureClass += "prose_only_next_steps"
}
if (-not $prepareHash.ContainsKey("commit_tool") -or -not $prepareHash.ContainsKey("ingest_tool")) {
    $failureClass += "missing_followup_tool_names"
}

$patchResult = @{
    attempted = $false
    route = $resolvedUploadUrl
    method = [string]$prepare.upload_method
    request_headers = Redact-Headers $headers
    status = $null
    response_body = $null
    client_behavior = $null
}

if ($AttemptPatch) {
    if (-not $isAbsoluteUrl -and -not $BaseUploadOrigin) {
        $patchResult.client_behavior = "not_sent_relative_upload_url_has_no_origin"
    } else {
        $patchResult.attempted = $true
        try {
            $body = [System.Text.Encoding]::UTF8.GetBytes("hello world!")
            $response = Invoke-WebRequest -Method Patch -Uri $resolvedUploadUrl -Headers $headers -Body $body
            $patchResult.status = [int]$response.StatusCode
            $patchResult.response_body = [string]$response.Content
        } catch {
            $patchResult.client_behavior = "http_client_error"
            $errorResponse = $_.Exception.Response
            $patchResult.status = Read-WebErrorStatus $errorResponse
            $body = Read-WebErrorBody $errorResponse
            $patchResult.response_body = $(if ($body) { $body } else { [string]$_.Exception.Message })
        }
    }
}

$cleanupResult = @{
    attempted = $false
    tool = "document_upload_abort"
    upload_id = [string]$prepare.upload_id
    status = $null
    error = $null
}
if ($McpUrl -and $prepare.upload_id) {
    $cleanupResult.attempted = $true
    try {
        $null = Invoke-McpTool "document_upload_abort" @{
            upload_id = [string]$prepare.upload_id
        }
        $cleanupResult.status = "aborted"
    } catch {
        $cleanupResult.status = "failed"
        $cleanupResult.error = [string]$_.Exception.Message
    }
}

$routeKind = [string]$prepare.route_kind
$hasUsableUploadUrl = $isAbsoluteUrl -or ($routeKind -eq "direct_local" -and $prepareHash.ContainsKey("upload_path"))
$contractV2Ready = ($missingV2Fields.Count -eq 0 -and $hasUsableUploadUrl -and ([string]$prepare.upload_method) -eq "PATCH")
$diagnosis = if ($contractV2Ready) {
    "Contract has v2 fields. Relay/public clients must use an absolute upload_url; direct_local clients may resolve upload_path against their configured Solo HTTP origin."
} else {
    "Contract is not v2-ready because agents cannot safely infer route/auth/follow-up actions from this response."
}
$summary = [ordered]@{
    repro = "document_upload_prepare_contract_v2"
    source = $(if ($PrepareResponsePath) { "prepare_response_path" } elseif ($McpUrl) { "mcp_url" } elseif ($LegacySample) { "built_in_legacy_local_sample" } else { "built_in_direct_local_v2_sample" })
    upload_id = [string]$prepare.upload_id
    upload_url = $uploadUrl
    resolved_upload_url = $resolvedUploadUrl
    upload_method = [string]$prepare.upload_method
    contract_v2_ready = $contractV2Ready
    missing_v2_fields = $missingV2Fields
    failure_class = $failureClass
    patch = $patchResult
    cleanup = $cleanupResult
    current_diagnosis = $diagnosis
}

$summary | ConvertTo-Json -Depth 12

if ($contractV2Ready -or $AllowKnownGap) {
    exit 0
}

exit 1
