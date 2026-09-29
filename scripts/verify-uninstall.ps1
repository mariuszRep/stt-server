$ErrorActionPreference = 'Stop'
$data = 'C:\ProgramData\OpenVibeAI\STT Server Next'
$model = Join-Path $data 'models\4b50b6dd862bf6e346929aaf4f5eaacec003bfa3f56462d6c874b41ef2f38795.gguf'
[pscustomobject]@{
    service_present = $null -ne (Get-Service OpenVibeSttNext -ErrorAction SilentlyContinue)
    binary_present = Test-Path 'C:\Program Files\OpenVibeAI\STT Server Next\stt-server-next.exe'
    state_present = Test-Path (Join-Path $data 'state.db')
    model_present = Test-Path $model
    model_size = if (Test-Path $model) { (Get-Item $model).Length } else { 0 }
} | ConvertTo-Json | Set-Content -LiteralPath 'D:\Users\mariu\Projects\stt-server-next\docs\uninstall-result.json'
