<#
Builds tests/fixtures/two-speakers-16k.wav (and the sidecar two-speakers-16k.json) with Windows'
built-in text-to-speech: two different voices alternating A/B/A/B with 400 ms of silence between
turns. The sidecar records which voice speaks when, so tests/speaker_binary.rs and src/speaker/
can check that embeddings separate the voices and that clustering recovers the script.

Run it with PowerShell 7 (`pwsh`), where System.Speech also offers the OneCore voices:

    pwsh -File scripts/make_two_speaker_fixture.ps1

The default pair is Microsoft George (male, en-GB) and Microsoft Zira Desktop (female, en-US);
Microsoft David Desktop, the other common male voice, is not installed on every machine, so pass
-VoiceA "Microsoft David Desktop" where it is.
#>
param(
    [string]$VoiceA = 'Microsoft George',
    [string]$VoiceB = 'Microsoft Zira Desktop',
    [string]$OutDir = (Join-Path $PSScriptRoot '..\tests\fixtures')
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Speech

$script = @(
    @{ voice = $VoiceA; speaker = 'A'; text = 'I think it will rain this afternoon.' },
    @{ voice = $VoiceB; speaker = 'B'; text = 'Then I will bring my umbrella along.' },
    @{ voice = $VoiceA; speaker = 'A'; text = 'Good, we can leave after the meeting.' },
    @{ voice = $VoiceB; speaker = 'B'; text = 'Let us meet by the front door.' }
)
$rate = 16000
$gapSamples = [int]($rate * 0.4)
$format = [System.Speech.AudioFormat.SpeechAudioFormatInfo]::new($rate, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono)
$synth = [System.Speech.Synthesis.SpeechSynthesizer]::new()
$temp = Join-Path ([System.IO.Path]::GetTempPath()) ("two-speakers-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temp | Out-Null

$pcm = [System.IO.MemoryStream]::new()
$segments = @()
try {
    foreach ($turn in $script) {
        $synth.SelectVoice($turn.voice)
        $path = Join-Path $temp "turn$($segments.Count).wav"
        $synth.SetOutputToWaveFile($path, $format)
        $synth.Speak($turn.text)
        $synth.SetOutputToNull()
        # A canonical 44-byte header precedes the 16-bit mono samples.
        $bytes = [System.IO.File]::ReadAllBytes($path)
        $samples = ($bytes.Length - 44) / 2
        if ($pcm.Length -gt 0) { $pcm.Write([byte[]]::new($gapSamples * 2), 0, $gapSamples * 2) }
        $start = $pcm.Length / 2 / $rate
        $pcm.Write($bytes, 44, $bytes.Length - 44)
        $segments += [ordered]@{ speaker = $turn.speaker; start = [math]::Round($start, 3); end = [math]::Round($start + $samples / $rate, 3) }
    }
}
finally {
    $synth.Dispose()
    Remove-Item -Recurse -Force $temp
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$wav = [System.IO.MemoryStream]::new()
$writer = [System.IO.BinaryWriter]::new($wav)
$writer.Write([System.Text.Encoding]::ASCII.GetBytes('RIFF')); $writer.Write([int](36 + $pcm.Length))
$writer.Write([System.Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $writer.Write([int]16)
$writer.Write([int16]1); $writer.Write([int16]1); $writer.Write([int]$rate); $writer.Write([int]($rate * 2)); $writer.Write([int16]2); $writer.Write([int16]16)
$writer.Write([System.Text.Encoding]::ASCII.GetBytes('data')); $writer.Write([int]$pcm.Length); $writer.Write($pcm.ToArray())
$writer.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $OutDir 'two-speakers-16k.wav'), $wav.ToArray())
[ordered]@{ voices = [ordered]@{ A = $VoiceA; B = $VoiceB }; duration = [math]::Round($pcm.Length / 2 / $rate, 3); segments = $segments } |
    ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 (Join-Path $OutDir 'two-speakers-16k.json')
Write-Host "two-speakers-16k.wav: $([math]::Round($pcm.Length / 2 / $rate, 2)) s"
