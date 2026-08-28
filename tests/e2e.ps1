$ErrorActionPreference = "Stop"

function Assert-True {
    param(
        [bool]$Condition,
        [string]$Message
    )
    if (-not $Condition) {
        throw "Assertion failed: $Message"
    }
}

function Invoke-JsonPost {
    param(
        [string]$Path,
        [hashtable]$Body
    )
    Invoke-RestMethod `
        -Method Post `
        -Uri "$($script:BaseUri)$Path" `
        -ContentType "application/json" `
        -Body ($Body | ConvertTo-Json -Compress)
}

function Get-ResponseText {
    param($Response)

    if ($Response.Content -is [byte[]]) {
        return [System.Text.Encoding]::UTF8.GetString($Response.Content)
    }
    return [string]$Response.Content
}

function ConvertFrom-Ndjson {
    param([string]$Text)

    $Text -split "\r?\n" |
        Where-Object { $_.Trim() } |
        ForEach-Object { $_ | ConvertFrom-Json }
}

$binary = (Resolve-Path "target/release/lcr.exe").Path
$listenHost = "127.0.0.1"
$listenPort = 19527
$listenAddress = "${listenHost}:$listenPort"
$script:BaseUri = "http://$listenAddress"
$testRoot = Join-Path $env:RUNNER_TEMP ("lcr-e2e-" + [guid]::NewGuid().ToString("N"))
$serverStdout = Join-Path $testRoot "server.stdout.log"
$serverStderr = Join-Path $testRoot "server.stderr.log"
$server = $null
$failed = $false

New-Item -ItemType Directory -Path $testRoot | Out-Null

try {
    $helpOutput = (& $binary --help | Out-String)
    Assert-True ($LASTEXITCODE -eq 0) "--help should exit successfully"
    Assert-True ($helpOutput.Contains("Usage:")) "--help should contain usage"
    Assert-True ($helpOutput.Contains("/exec/stream")) "--help should describe execution endpoints"
    Assert-True ($helpOutput.Contains("script_mode")) "--help should describe script modes"
    Assert-True ($helpOutput.Contains("no authentication")) "--help should include the security warning"

    $helpCommandOutput = (& $binary help | Out-String)
    Assert-True ($LASTEXITCODE -eq 0) "help command should exit successfully"
    Assert-True ($helpCommandOutput.Contains("0.0.0.0:9527")) "help command should describe the listener"

    $server = Start-Process `
        -FilePath $binary `
        -ArgumentList @("serve", "--listen", $listenAddress) `
        -PassThru `
        -RedirectStandardOutput $serverStdout `
        -RedirectStandardError $serverStderr

    $ready = $false
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        if ($server.HasExited) {
            throw "lcr exited before becoming ready"
        }
        $client = [System.Net.Sockets.TcpClient]::new()
        try {
            $client.Connect($listenHost, $listenPort)
            $ready = $true
            break
        }
        catch {
            Start-Sleep -Milliseconds 100
        }
        finally {
            $client.Dispose()
        }
    }
    Assert-True $ready "lcr should listen on port 9527"

    $cmdResult = Invoke-JsonPost "/exec" @{
        command = "echo cmd-ok"
    }
    Assert-True $cmdResult.ok "cmd execution should succeed"
    Assert-True ($cmdResult.stdout.Contains("cmd-ok")) "cmd stdout should be returned"

    $workingDirectory = Join-Path $testRoot "working-directory"
    New-Item -ItemType Directory -Path $workingDirectory | Out-Null
    $cwdResult = Invoke-JsonPost "/exec" @{
        command = "cd"
        cwd = $workingDirectory
    }
    Assert-True $cwdResult.ok "execution with a working directory should succeed"
    Assert-True `
        ([string]::Equals($cwdResult.stdout.Trim(), $workingDirectory, [System.StringComparison]::OrdinalIgnoreCase)) `
        "command should run in the requested working directory"

    $pwshResult = Invoke-JsonPost "/exec" @{
        command = "Write-Output 'pwsh-ok'"
        interpreter = "pwsh"
    }
    Assert-True $pwshResult.ok "pwsh execution should succeed"
    Assert-True ($pwshResult.stdout.Contains("pwsh-ok")) "pwsh stdout should be returned"

    $python = (Get-Command python.exe).Source
    $customResult = Invoke-JsonPost "/exec" @{
        command = "print('custom-ok')"
        interpreter = $python
        script_mode = "file"
    }
    Assert-True $customResult.ok "absolute interpreter execution should succeed"
    Assert-True ($customResult.stdout.Contains("custom-ok")) "custom interpreter stdout should be returned"

    $multilineResult = Invoke-JsonPost "/exec" @{
        command = "@echo off`r`nset E2E_VALUE=multiline-ok`r`necho %E2E_VALUE%"
        interpreter = "cmd"
        script_mode = "auto"
    }
    Assert-True $multilineResult.ok "automatic multiline script execution should succeed"
    Assert-True ($multilineResult.stdout.Contains("multiline-ok")) "multiline stdout should be returned"

    $forcedFileResult = Invoke-JsonPost "/exec" @{
        command = "echo forced-file-ok"
        script_mode = "file"
    }
    Assert-True $forcedFileResult.ok "forced file execution should succeed"
    Assert-True ($forcedFileResult.stdout.Contains("forced-file-ok")) "forced file stdout should be returned"

    $streamResponse = Invoke-WebRequest `
        -Method Post `
        -Uri "$($script:BaseUri)/exec/stream" `
        -ContentType "application/json" `
        -Body (@{ command = "echo stream-out & echo stream-err 1>&2" } | ConvertTo-Json -Compress)
    $streamText = Get-ResponseText $streamResponse
    $events = @(ConvertFrom-Ndjson $streamText)
    $streamStdout = (($events | Where-Object { $_.type -eq "stdout" } | ForEach-Object { $_.data }) -join "")
    $streamStderr = (($events | Where-Object { $_.type -eq "stderr" } | ForEach-Object { $_.data }) -join "")
    $exitEvent = $events | Where-Object { $_.type -eq "exit" } | Select-Object -Last 1
    Assert-True `
        ($streamStdout.Contains("stream-out")) `
        "streaming stdout should be returned; response: $streamText"
    Assert-True `
        ($streamStderr.Contains("stream-err")) `
        "streaming stderr should be returned; response: $streamText"
    Assert-True ($exitEvent.exit_code -eq 0) "streaming exit event should be returned"

    $streamTimeoutResponse = Invoke-WebRequest `
        -Method Post `
        -Uri "$($script:BaseUri)/exec/stream" `
        -ContentType "application/json" `
        -Body (@{ command = "ping 127.0.0.1 -n 6 >nul"; timeout = 100 } | ConvertTo-Json -Compress)
    $streamTimeoutText = Get-ResponseText $streamTimeoutResponse
    $timeoutEvents = @(ConvertFrom-Ndjson $streamTimeoutText)
    $streamTimeoutEvent = $timeoutEvents | Where-Object { $_.type -eq "timeout" } | Select-Object -Last 1
    Assert-True ($streamTimeoutEvent.timeout -eq 100) "streaming timeout event should be returned"

    $timeoutResult = Invoke-JsonPost "/exec" @{
        command = "ping 127.0.0.1 -n 6 >nul"
        timeout = 100
    }
    Assert-True $timeoutResult.timed_out "long-running command should time out"
    Assert-True (-not $timeoutResult.ok) "timed-out command should not be successful"

    $sourceFile = Join-Path $testRoot "source.bin"
    $uploadedFile = Join-Path $testRoot "uploaded.bin"
    $downloadedFile = Join-Path $testRoot "downloaded.bin"
    [System.IO.File]::WriteAllBytes($sourceFile, [byte[]](0, 1, 2, 3, 127, 128, 254, 255))

    $uploadResult = Invoke-RestMethod `
        -Method Post `
        -Uri "$($script:BaseUri)/upload" `
        -ContentType "application/octet-stream" `
        -Headers @{ "X-File-Path" = $uploadedFile } `
        -InFile $sourceFile
    Assert-True $uploadResult.ok "file upload should succeed"
    Assert-True ($uploadResult.bytes -eq 8) "uploaded byte count should match"

    $conflictStatus = 0
    try {
        Invoke-RestMethod `
            -Method Post `
            -Uri "$($script:BaseUri)/upload" `
            -ContentType "application/octet-stream" `
            -Headers @{ "X-File-Path" = $uploadedFile } `
            -InFile $sourceFile | Out-Null
    }
    catch {
        $conflictStatus = [int]$_.Exception.Response.StatusCode
    }
    Assert-True ($conflictStatus -eq 409) "upload should not overwrite an existing file"

    Invoke-WebRequest `
        -Method Post `
        -Uri "$($script:BaseUri)/download" `
        -ContentType "application/json" `
        -Body (@{ path = $uploadedFile } | ConvertTo-Json -Compress) `
        -OutFile $downloadedFile
    $sourceHash = (Get-FileHash -Algorithm SHA256 $sourceFile).Hash
    $downloadHash = (Get-FileHash -Algorithm SHA256 $downloadedFile).Hash
    Assert-True ($sourceHash -eq $downloadHash) "downloaded file should match uploaded content"

    $serverLog = Get-Content -Path $serverStdout -Raw
    Assert-True `
        ($serverLog -match "(?m)^\[info\] \d{2}:\d{2}:\d{2} lcr listening on http://") `
        "server logs should contain the standard level and timestamp prefix"

    Write-Host "All lcr E2E tests passed."
}
catch {
    $failed = $true
    throw
}
finally {
    if ($null -ne $server -and -not $server.HasExited) {
        Stop-Process -Id $server.Id -Force
        Wait-Process -Id $server.Id -ErrorAction SilentlyContinue
    }
    if ($failed) {
        if (Test-Path $serverStdout) {
            Write-Host "--- lcr stdout ---"
            Get-Content $serverStdout
        }
        if (Test-Path $serverStderr) {
            Write-Host "--- lcr stderr ---"
            Get-Content $serverStderr
        }
    }
    Remove-Item -Path $testRoot -Recurse -Force -ErrorAction SilentlyContinue
}
