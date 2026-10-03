$ErrorActionPreference = 'Stop'
$data = 'C:\ProgramData\OpenVibeAI\STT Server'
$model = Join-Path $data 'models\4b50b6dd862bf6e346929aaf4f5eaacec003bfa3f56462d6c874b41ef2f38795.gguf'
[pscustomobject]@{
    service_present = $null -ne (Get-Service OpenVibeSttServer -ErrorAction SilentlyContinue)
    binary_present = Test-Path 'C:\Program Files\OpenVibeAI\STT Server\stt-server.exe'
    state_present = Test-Path (Join-Path $data 'state.db')
    model_present = Test-Path $model
    model_size = if (Test-Path $model) { (Get-Item $model).Length } else { 0 }
} | ConvertTo-Json | Set-Content -LiteralPath 'D:\Users\mariu\Projects\voice-typer\stt-server\docs\uninstall-result.json'
