$ErrorActionPreference = 'Stop'
$name = 'OpenVibeSttNext'
$before = Get-CimInstance Win32_Service -Filter "Name='$name'"
if ($null -eq $before -or $before.State -ne 'Running' -or $before.ProcessId -le 0) {
    throw 'The test service is not running.'
}
$oldPid = [int]$before.ProcessId
Stop-Process -Id $oldPid -Force
$after = $null
for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Seconds 1
    $current = Get-CimInstance Win32_Service -Filter "Name='$name'"
    if ($current.State -eq 'Running' -and $current.ProcessId -gt 0 -and $current.ProcessId -ne $oldPid) {
        $after = $current
        break
    }
}
if ($null -eq $after) { throw 'Service did not restart with a new process ID.' }
$token = Get-Content 'C:\ProgramData\OpenVibeAI\STT Server\auth.token' -Raw
$headers = @{ Authorization = "Bearer $token" }
$ready = $null
for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Seconds 1
    try {
        $ready = Invoke-RestMethod 'http://127.0.0.1:54321/readiness' -Headers $headers
        if ($ready.status -eq 'ready') { break }
    } catch { }
}
if ($null -eq $ready -or $ready.status -ne 'ready') {
    throw 'Service restarted but did not reload its selected model.'
}
[pscustomobject]@{
    service = $name
    old_pid = $oldPid
    new_pid = [int]$after.ProcessId
    status = $ready.status
    model = $ready.model
    backend = $ready.backend.observed_backend
} | ConvertTo-Json | Set-Content -LiteralPath 'D:\Users\mariu\Projects\voice-typer\stt-server\docs\service-recovery-result.json'
