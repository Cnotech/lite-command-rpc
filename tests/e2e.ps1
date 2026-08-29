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

function Start-E2ECase {
    param([string]$Name)

    $script:CaseCount++
    $script:CurrentCase = $Name
    $script:CurrentCaseTimer = [System.Diagnostics.Stopwatch]::StartNew()
    Write-Host ("[e2e] START {0:D2}/{1:D2} {2}" -f `
        $script:CaseCount, $script:ExpectedCaseCount, $Name)
}

function Complete-E2ECase {
    $script:CurrentCaseTimer.Stop()
    $script:PassedCaseCount++
    Write-Host ("[e2e] PASS  {0:D2}/{1:D2} {2} ({3} ms)" -f `
        $script:CaseCount, $script:ExpectedCaseCount, $script:CurrentCase, `
        $script:CurrentCaseTimer.ElapsedMilliseconds)
    $script:CurrentCase = $null
    $script:CurrentCaseTimer = $null
}

function Get-FreeTcpPort {
    $listener = [System.Net.Sockets.TcpListener]::new(
        [System.Net.IPAddress]::Loopback,
        0
    )
    try {
        $listener.Start()
        return ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    }
    finally {
        $listener.Stop()
    }
}

$binary = (Resolve-Path "target/release/lcr.exe").Path
$listenHost = "127.0.0.1"
$listenPort = Get-FreeTcpPort
$listenAddress = "${listenHost}:$listenPort"
$script:BaseUri = "http://$listenAddress"
$testRoot = Join-Path $env:RUNNER_TEMP ("lcr-e2e-" + [guid]::NewGuid().ToString("N"))
$serverStdout = Join-Path $testRoot "server.stdout.log"
$serverStderr = Join-Path $testRoot "server.stderr.log"
$server = $null
$failed = $false
$script:ExpectedCaseCount = 24
$script:CaseCount = 0
$script:PassedCaseCount = 0
$script:CurrentCase = $null
$script:CurrentCaseTimer = $null
$detachedChildPid = $null
$detachedChildName = $null
$totalTimer = [System.Diagnostics.Stopwatch]::StartNew()

New-Item -ItemType Directory -Path $testRoot | Out-Null

try {
    Start-E2ECase "CLI help and command metadata"
    $helpOutput = (& $binary --help | Out-String)
    Assert-True ($LASTEXITCODE -eq 0) "--help should exit successfully"
    Assert-True ($helpOutput.Contains("Usage:")) "--help should contain usage"
    Assert-True ($helpOutput.Contains("/exec/stream")) "--help should describe execution endpoints"
    Assert-True ($helpOutput.Contains("/spawn/result")) "--help should describe asynchronous execution"
    Assert-True ($helpOutput.Contains("/spawn/terminate")) "--help should describe spawn termination"
    Assert-True ($helpOutput.Contains("/screenshot")) "--help should describe desktop endpoints"
    Assert-True ($helpOutput.Contains("script_mode")) "--help should describe script modes"
    Assert-True ($helpOutput.Contains("no authentication")) "--help should include the security warning"

    $helpCommandOutput = (& $binary help | Out-String)
    Assert-True ($LASTEXITCODE -eq 0) "help command should exit successfully"
    Assert-True ($helpCommandOutput.Contains("0.0.0.0:9527")) "help command should describe the listener"
    Complete-E2ECase

    Start-E2ECase "Server startup with custom listen address"
    $server = Start-Process `
        -FilePath $binary `
        -ArgumentList @("serve", "--listen", $listenAddress, "--log-level", "debug") `
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
    Assert-True $ready "lcr should listen on $listenAddress"
    Complete-E2ECase

    Start-E2ECase "Direct program execution with Unicode argument"
    $python = (Get-Command python.exe).Source
    $directResult = Invoke-JsonPost "/exec" @{
        program = $python
        args = @("-c", "import sys; print('unicode-direct-ok' if sys.argv[1] == '搜狗拼音路径' else 'unicode-direct-bad')", "搜狗拼音路径")
    }
    Assert-True $directResult.ok "direct Unicode program execution should succeed"
    Assert-True ($directResult.stdout.Contains("unicode-direct-ok")) "Unicode argument should reach the program intact"
    Complete-E2ECase

    Start-E2ECase "Unicode stdout"
    $unicodeOutput = Invoke-JsonPost "/exec" @{
        command = "echo 搜狗拼音"
        output_encoding = "utf8"
    }
    Assert-True $unicodeOutput.ok "Unicode command should succeed"
    Assert-True ($unicodeOutput.stdout.Contains("搜狗拼音")) "Unicode stdout should be preserved"
    Complete-E2ECase

    Start-E2ECase "Output encoding validation"
    $invalidEncodingStatus = 0
    try {
        Invoke-RestMethod `
            -Method Post `
            -Uri "$($script:BaseUri)/exec" `
            -ContentType "application/json" `
            -Body (@{ command = "echo hi"; output_encoding = "unknown" } | ConvertTo-Json -Compress) | Out-Null
    }
    catch {
        $invalidEncodingStatus = [int]$_.Exception.Response.StatusCode
    }
    Assert-True ($invalidEncodingStatus -eq 400) "unknown output encoding should be rejected"
    Complete-E2ECase

    Start-E2ECase "Empty request without Content-Length"
    foreach ($emptyPath in @("/windows", "/screenshot")) {
        $rawClient = [System.Net.Sockets.TcpClient]::new($listenHost, $listenPort)
        try {
            $rawStream = $rawClient.GetStream()
            $requestBytes = [System.Text.Encoding]::ASCII.GetBytes(
                "POST $emptyPath HTTP/1.1`r`nHost: $listenAddress`r`nConnection: close`r`n`r`n"
            )
            $rawStream.Write($requestBytes, 0, $requestBytes.Length)
            $responseBuffer = [System.IO.MemoryStream]::new()
            $rawStream.CopyTo($responseBuffer)
            $responseBytes = $responseBuffer.ToArray()
        }
        finally {
            $rawClient.Dispose()
        }
        $headerEnd = -1
        for ($index = 0; $index -le $responseBytes.Length - 4; $index++) {
            if ($responseBytes[$index] -eq 13 -and $responseBytes[$index + 1] -eq 10 -and `
                $responseBytes[$index + 2] -eq 13 -and $responseBytes[$index + 3] -eq 10) {
                $headerEnd = $index
                break
            }
        }
        Assert-True ($headerEnd -ge 0) "$emptyPath response should contain a complete HTTP header"
        $responseHeaders = [System.Text.Encoding]::ASCII.GetString($responseBytes, 0, $headerEnd)
        Assert-True `
            ($responseHeaders -match "^HTTP/1\.1 (200|500) ") `
            "$emptyPath should be routed without requiring Content-Length"
    }
    Complete-E2ECase

    Start-E2ECase "Unsupported transfer-encoded request bodies"
    $transferTarget = Join-Path $testRoot "transfer-encoded-upload.bin"
    $transferHeaderCases = @(
        "Transfer-Encoding: chunked`r`n",
        "Transfer-Encoding: identity`r`n",
        "Transfer-Encoding: chunked`r`nTransfer-Encoding: identity`r`n"
    )
    foreach ($transferHeaders in $transferHeaderCases) {
        $rawClient = [System.Net.Sockets.TcpClient]::new($listenHost, $listenPort)
        try {
            $rawStream = $rawClient.GetStream()
            $requestBytes = [System.Text.Encoding]::ASCII.GetBytes(
                "POST /upload HTTP/1.1`r`nHost: $listenAddress`r`n" +
                $transferHeaders + "X-File-Path: $transferTarget`r`n" +
                "Connection: close`r`n`r`n4`r`ntest`r`n0`r`n`r`n"
            )
            $rawStream.Write($requestBytes, 0, $requestBytes.Length)
            $responseBuffer = [System.IO.MemoryStream]::new()
            $rawStream.CopyTo($responseBuffer)
            $responseBytes = $responseBuffer.ToArray()
        }
        finally {
            $rawClient.Dispose()
        }
        $responseText = [System.Text.Encoding]::UTF8.GetString($responseBytes)
        Assert-True `
            ($responseText.StartsWith("HTTP/1.1 400 Bad Request")) `
            "unsupported or duplicate Transfer-Encoding should be rejected"
        Assert-True `
            (-not (Test-Path -LiteralPath $transferTarget)) `
            "rejected transfer-encoded upload should not create a destination file"
    }
    Complete-E2ECase

    Start-E2ECase "CMD execution"
    $cmdResult = Invoke-JsonPost "/exec" @{
        command = "echo cmd-ok"
    }
    Assert-True $cmdResult.ok "cmd execution should succeed"
    Assert-True ($cmdResult.stdout.Contains("cmd-ok")) "cmd stdout should be returned"
    Complete-E2ECase

    Start-E2ECase "Asynchronous spawn and result polling"
    $spawnResult = Invoke-JsonPost "/spawn" @{
        command = "echo spawn-out & echo spawn-err 1>&2 & ping 127.0.0.1 -n 2 >nul"
        timeout = 5000
    }
    Assert-True `
        (-not [string]::IsNullOrWhiteSpace([string]$spawnResult.session_id)) `
        "spawn should return a session ID"
    Assert-True ($spawnResult.pid -gt 0) "spawn should return a PID"

    $spawnQuery = $null
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        $spawnQuery = Invoke-JsonPost "/spawn/result" @{
            session_id = $spawnResult.session_id
        }
        if ($spawnQuery.status -notin @("starting", "running")) {
            break
        }
        Start-Sleep -Milliseconds 50
    }
    Assert-True ($spawnQuery.status -eq "exited") "spawned command should finish normally"
    Assert-True ($spawnQuery.exit_code -eq 0) "spawned command should return exit code zero"
    Assert-True ($spawnQuery.stdout.Contains("spawn-out")) "spawn result should contain stdout"
    Assert-True ($spawnQuery.stderr.Contains("spawn-err")) "spawn result should contain stderr"
    Assert-True (-not $spawnQuery.stdout_truncated) "small stdout should not be truncated"
    $spawnDelta = Invoke-JsonPost "/spawn/result" @{
        session_id = $spawnResult.session_id
        stdout_offset = $spawnQuery.stdout_next_offset
        stderr_offset = $spawnQuery.stderr_next_offset
    }
    Assert-True ($spawnDelta.stdout -eq "") "spawn stdout offset should return only new data"
    Assert-True ($spawnDelta.stderr -eq "") "spawn stderr offset should return only new data"
    $completedTerminate = Invoke-JsonPost "/spawn/terminate" @{
        session_id = $spawnResult.session_id
    }
    Assert-True ($completedTerminate.status -eq "exited") "terminate should preserve a completed task"
    Complete-E2ECase

    Start-E2ECase "Asynchronous spawn termination"
    $terminateSpawn = Invoke-JsonPost "/spawn" @{
        command = '$child = Start-Process ping.exe -ArgumentList @("127.0.0.1", "-n", "30") -WindowStyle Hidden -PassThru; Write-Output "child-pid=$($child.Id)"; Wait-Process -Id $child.Id'
        interpreter = "pwsh"
        timeout = 60000
    }
    $childPid = $null
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        $runningResult = Invoke-JsonPost "/spawn/result" @{
            session_id = $terminateSpawn.session_id
        }
        if ($runningResult.stdout -match "child-pid=(\d+)") {
            $childPid = [int]$Matches[1]
            break
        }
        Start-Sleep -Milliseconds 50
    }
    Assert-True ($childPid -gt 0) "spawn should report its child process PID"

    $invalidOffsetStatus = 200
    try {
        Invoke-WebRequest `
            -Method Post `
            -Uri "$($script:BaseUri)/spawn/terminate" `
            -ContentType "application/json" `
            -Body (@{ session_id = $terminateSpawn.session_id; stdout_offset = 999999 } | ConvertTo-Json -Compress) |
            Out-Null
    }
    catch {
        $invalidOffsetStatus = [int]$_.Exception.Response.StatusCode
    }
    Assert-True ($invalidOffsetStatus -eq 400) "invalid terminate offset should be rejected"
    Assert-True `
        ($null -ne (Get-Process -Id $childPid -ErrorAction SilentlyContinue)) `
        "invalid terminate offset should not stop the task"

    $terminateResult = Invoke-JsonPost "/spawn/terminate" @{
        session_id = $terminateSpawn.session_id
    }
    Assert-True ($terminateResult.status -eq "terminated") "terminated spawn should report terminated"
    Assert-True ($null -eq $terminateResult.exit_code) "terminated spawn should not report an exit code"
    Assert-True ($terminateResult.stdout.Contains("child-pid=")) "termination should return captured stdout"
    Start-Sleep -Milliseconds 50
    Assert-True `
        ($null -eq (Get-Process -Id $terminateSpawn.pid -ErrorAction SilentlyContinue)) `
        "terminated spawn process should no longer exist"
    Assert-True `
        ($null -eq (Get-Process -Id $childPid -ErrorAction SilentlyContinue)) `
        "terminated spawn child process should no longer exist"
    $terminatedQuery = Invoke-JsonPost "/spawn/result" @{
        session_id = $terminateSpawn.session_id
    }
    Assert-True ($terminatedQuery.status -eq "terminated") "result should preserve terminated status"
    Complete-E2ECase

    Start-E2ECase "Detached child survives wrapper exit"
    $detachedPort = Get-FreeTcpPort
    $detachedChildName = "lcr-detached-" + [guid]::NewGuid().ToString("N")
    $detachedBinary = Join-Path $testRoot ($detachedChildName + ".exe")
    Copy-Item -LiteralPath $binary -Destination $detachedBinary
    $detachedSpawn = Invoke-JsonPost "/spawn" @{
        command = 'start "" /b "' + $detachedBinary + '" serve --listen 127.0.0.1:' + $detachedPort
        interpreter = "cmd"
        script_mode = "file"
        detached = $true
        timeout = 5000
    }
    $detachedResult = $null
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        $detachedResult = Invoke-JsonPost "/spawn/result" @{
            session_id = $detachedSpawn.session_id
        }
        if ($detachedResult.status -notin @("starting", "running")) {
            break
        }
        Start-Sleep -Milliseconds 50
    }
    Assert-True ($detachedResult.status -eq "exited") "detached wrapper should exit normally"
    Assert-True ($detachedResult.stdout -eq "") "detached execution should not capture stdout"
    $detachedReady = $false
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        $detachedClient = [System.Net.Sockets.TcpClient]::new()
        try {
            $connectTask = $detachedClient.ConnectAsync($listenHost, $detachedPort)
            if ($connectTask.Wait(100) -and $detachedClient.Connected) {
                $detachedReady = $true
                break
            }
        }
        catch {
            Start-Sleep -Milliseconds 50
        }
        finally {
            $detachedClient.Dispose()
        }
    }
    Assert-True $detachedReady "detached child service should survive wrapper exit"
    $detachedChildPid = (Get-Process -Name $detachedChildName).Id
    Assert-True `
        ($null -ne (Get-Process -Id $detachedChildPid -ErrorAction SilentlyContinue)) `
        "detached child should survive wrapper exit"
    Stop-Process -Id $detachedChildPid -Force
    Wait-Process -Id $detachedChildPid -ErrorAction SilentlyContinue
    $detachedChildPid = $null
    $detachedChildName = $null
    Complete-E2ECase

    Start-E2ECase "Primary-screen PNG screenshot"
    $screenshotFile = Join-Path $testRoot "screenshot.png"
    $screenshotStatus = 200
    try {
        Invoke-WebRequest `
            -Method Post `
            -Uri "$($script:BaseUri)/screenshot" `
            -ContentType "application/json" `
            -Body "{}" `
            -OutFile $screenshotFile
    }
    catch {
        $screenshotStatus = [int]$_.Exception.Response.StatusCode
    }
    Assert-True `
        ($screenshotStatus -in @(200, 500)) `
        "screenshot should return PNG or report an unavailable display"
    if ($screenshotStatus -eq 200) {
        $screenshotBytes = [System.IO.File]::ReadAllBytes($screenshotFile)
        Assert-True ($screenshotBytes.Length -gt 8) "screenshot should contain PNG data"
        Assert-True `
            ($screenshotBytes[0] -eq 0x89 -and $screenshotBytes[1] -eq 0x50 -and `
             $screenshotBytes[2] -eq 0x4e -and $screenshotBytes[3] -eq 0x47) `
            "screenshot should have a PNG signature"
    }
    Complete-E2ECase

    Start-E2ECase "Top-level window enumeration"
    $windowsResult = Invoke-JsonPost "/windows" @{}
    Assert-True ($null -ne $windowsResult.windows) "windows response should contain a window array"
    $firstWindow = @($windowsResult.windows) | Select-Object -First 1
    if ($null -ne $firstWindow) {
        Assert-True ($firstWindow.hwnd.StartsWith("0x")) "window HWND should be hexadecimal"
        Assert-True ($firstWindow.pid -ge 0) "window should include a PID"
        Assert-True ($null -ne $firstWindow.rect) "window should include its rectangle"
    }
    Complete-E2ECase

    Start-E2ECase "Control request validation"
    $controlStatus = 0
    try {
        Invoke-JsonPost "/control" @{ actions = @() } | Out-Null
    }
    catch {
        $controlStatus = [int]$_.Exception.Response.StatusCode
    }
    Assert-True ($controlStatus -eq 400) "control should reject an empty action list"
    $controlDelayStatus = 0
    try {
        Invoke-JsonPost "/control" @{
            actions = @(@{ type = "keyboard"; key = "G" })
            delay = 5001
        } | Out-Null
    }
    catch {
        $controlDelayStatus = [int]$_.Exception.Response.StatusCode
    }
    Assert-True ($controlDelayStatus -eq 400) "control should reject an excessive delay"
    $controlTotalDelayStatus = 0
    try {
        Invoke-JsonPost "/control" @{
            actions = @(1..8 | ForEach-Object { @{ type = "keyboard"; key = "G" } })
            delay = 5000
        } | Out-Null
    }
    catch {
        $controlTotalDelayStatus = [int]$_.Exception.Response.StatusCode
    }
    Assert-True `
        ($controlTotalDelayStatus -eq 400) `
        "control should reject an excessive total action delay"
    Complete-E2ECase

    Start-E2ECase "Working directory"
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
    Complete-E2ECase

    Start-E2ECase "PowerShell interpreter"
    $pwshResult = Invoke-JsonPost "/exec" @{
        command = "Write-Output 'pwsh-ok'"
        interpreter = "pwsh"
    }
    Assert-True $pwshResult.ok "pwsh execution should succeed"
    Assert-True ($pwshResult.stdout.Contains("pwsh-ok")) "pwsh stdout should be returned"
    Complete-E2ECase

    Start-E2ECase "Absolute custom interpreter with script file"
    $python = (Get-Command python.exe).Source
    $customResult = Invoke-JsonPost "/exec" @{
        command = "print('custom-ok')"
        interpreter = $python
        script_mode = "file"
    }
    Assert-True $customResult.ok "absolute interpreter execution should succeed"
    Assert-True ($customResult.stdout.Contains("custom-ok")) "custom interpreter stdout should be returned"
    Complete-E2ECase

    Start-E2ECase "Automatic multiline CMD script"
    $multilineResult = Invoke-JsonPost "/exec" @{
        command = "@echo off`r`nset E2E_VALUE=multiline-ok`r`necho %E2E_VALUE%"
        interpreter = "cmd"
        script_mode = "auto"
    }
    Assert-True $multilineResult.ok "automatic multiline script execution should succeed"
    Assert-True ($multilineResult.stdout.Contains("multiline-ok")) "multiline stdout should be returned"
    Complete-E2ECase

    Start-E2ECase "Forced temporary script file"
    $forcedFileResult = Invoke-JsonPost "/exec" @{
        command = "echo forced-file-ok"
        script_mode = "file"
    }
    Assert-True $forcedFileResult.ok "forced file execution should succeed"
    Assert-True ($forcedFileResult.stdout.Contains("forced-file-ok")) "forced file stdout should be returned"
    Complete-E2ECase

    Start-E2ECase "Streaming stdout, stderr, and exit event"
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
    Complete-E2ECase

    Start-E2ECase "Streaming timeout event"
    $streamTimeoutResponse = Invoke-WebRequest `
        -Method Post `
        -Uri "$($script:BaseUri)/exec/stream" `
        -ContentType "application/json" `
        -Body (@{ command = "ping 127.0.0.1 -n 6 >nul"; timeout = 100 } | ConvertTo-Json -Compress)
    $streamTimeoutText = Get-ResponseText $streamTimeoutResponse
    $timeoutEvents = @(ConvertFrom-Ndjson $streamTimeoutText)
    $streamTimeoutEvent = $timeoutEvents | Where-Object { $_.type -eq "timeout" } | Select-Object -Last 1
    Assert-True ($streamTimeoutEvent.timeout -eq 100) "streaming timeout event should be returned"
    Complete-E2ECase

    Start-E2ECase "Non-streaming command timeout"
    $timeoutResult = Invoke-JsonPost "/exec" @{
        command = "ping 127.0.0.1 -n 6 >nul"
        timeout = 100
    }
    Assert-True $timeoutResult.timed_out "long-running command should time out"
    Assert-True (-not $timeoutResult.ok) "timed-out command should not be successful"
    Complete-E2ECase

    Start-E2ECase "Binary upload, conflict, and download"
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

    Complete-E2ECase

    Start-E2ECase "Server log levels and execution lifecycle"
    $serverLog = Get-Content -Path $serverStdout -Raw
    Assert-True `
        ($serverLog -match "(?m)^\[info\] \d{2}:\d{2}:\d{2} lcr listening on http://") `
        "server logs should contain the standard level and timestamp prefix"
    Assert-True `
        ($serverLog -match "(?m)^\[debug\] \d{2}:\d{2}:\d{2} client connected:") `
        "debug logging should be enabled by --log-level debug"
    Assert-True `
        ($serverLog -match "(?m)^\[info\] \d{2}:\d{2}:\d{2} execution finished: exit_code=Some\(0\), timed_out=false") `
        "non-streaming execution should log its final status"
    Assert-True `
        ($serverLog -match "(?m)^\[info\] \d{2}:\d{2}:\d{2} stream stdout: .*stream-out") `
        "streaming stdout content should be logged"
    Assert-True `
        ($serverLog -match "(?m)^\[info\] \d{2}:\d{2}:\d{2} stream stderr: .*stream-err") `
        "streaming stderr content should be logged"
    Assert-True `
        ($serverLog -match "(?m)^\[info\] \d{2}:\d{2}:\d{2} stream finished: exit_code=Some\(0\), timed_out=false") `
        "streaming execution should log its exit status"
    Assert-True `
        ($serverLog -match "(?m)^\[info\] \d{2}:\d{2}:\d{2} stream finished: timed_out=true, timeout=100ms") `
        "streaming execution should log its timeout status"
    Complete-E2ECase

    $totalTimer.Stop()
    Assert-True `
        ($script:CaseCount -eq $script:ExpectedCaseCount) `
        "all expected E2E cases should be started"
    Assert-True `
        ($script:PassedCaseCount -eq $script:ExpectedCaseCount) `
        "all expected E2E cases should pass"
    Write-Host ("[e2e] SUMMARY {0}/{1} cases passed ({2} ms total)" -f `
        $script:PassedCaseCount, $script:CaseCount, $totalTimer.ElapsedMilliseconds)
}
catch {
    $failed = $true
    $totalTimer.Stop()
    if ($null -ne $script:CurrentCaseTimer) {
        $script:CurrentCaseTimer.Stop()
        Write-Host ("[e2e] FAIL  {0:D2}/{1:D2} {2} ({3} ms)" -f `
            $script:CaseCount, $script:ExpectedCaseCount, $script:CurrentCase, `
            $script:CurrentCaseTimer.ElapsedMilliseconds)
    }
    Write-Host ("[e2e] SUMMARY {0}/{1} cases passed before failure ({2} ms total)" -f `
        $script:PassedCaseCount, $script:CaseCount, $totalTimer.ElapsedMilliseconds)
    throw
}
finally {
    if ($null -ne $detachedChildPid) {
        Stop-Process -Id $detachedChildPid -Force -ErrorAction SilentlyContinue
        Wait-Process -Id $detachedChildPid -ErrorAction SilentlyContinue
    }
    if ($null -ne $detachedChildName) {
        Get-Process -Name $detachedChildName -ErrorAction SilentlyContinue |
            Stop-Process -Force -ErrorAction SilentlyContinue
    }
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
