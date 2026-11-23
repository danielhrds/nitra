#Requires -Version 5.1
<#
.SYNOPSIS
    Records a lossless screen clip (optional) and compares the quality of the
    hardware encoders on it at equal bitrates (PSNR, SSIM, VMAF).

.DESCRIPTION
    Follows docs/benchmark-protocol.md ("Encoder comparisons").

    -Record: captures -RecordSeconds of monitor -Monitor with ffmpeg
    (gfxcapture, Windows.Graphics.Capture, the app's default capture API)
    into a lossless RGB file (libx264rgb, qp 0) at a constant -Fps
    (repeating frames when the screen does not change). Run it while the
    scene to measure is on screen. (ddagrab was refused with "access
    denied" on the development machine.)

    Otherwise, from -Clip: converts the clip to raw NV12 references (BT.709,
    limited range, as the app's converter produces) at 1920x1080 and
    1280x720, encodes each reference with every -Encoders x -Kbps
    combination through `bench encode --source raw` (every frame encoded
    exactly once), decodes the result to raw NV12 without any range change,
    and compares it frame by frame with the reference. Results go to
    quality.csv and quality.json in -OutDir. Decoded and reference raw files
    are deleted at the end unless -KeepRaw.

    Needs ffmpeg with libvmaf (the gyan.dev essentials build has it). Keep
    this file ASCII-only: PowerShell 5.1 reads BOM-less files with the ANSI
    code page.

.EXAMPLE
    .\scripts\encode-quality.ps1 -Record -Clip D:\Projects\live\lab\quality\range.mkv -RecordSeconds 20

.EXAMPLE
    .\scripts\encode-quality.ps1 -Clip D:\Projects\live\lab\quality\range.mkv
#>
[CmdletBinding()]
param(
    # Lossless clip: written with -Record, read otherwise.
    [Parameter(Mandatory)] [string] $Clip,

    [switch] $Record,
    [ValidateRange(2, 120)] [int] $RecordSeconds = 20,
    # gfxcapture monitor index (0 = first monitor).
    [ValidateRange(0, 16)] [int] $Monitor = 0,

    [string[]] $Encoders = @('mf', 'amf'),
    # Bitrates (kbit/s) at 1920x1080.
    [int[]] $Kbps = @(6000, 10000, 20000),
    # Bitrates (kbit/s) at 1280x720 (empty: skip 720p).
    [int[]] $Kbps720 = @(6000),
    [ValidateRange(1, 240)] [int] $Fps = 60,

    # ffmpeg bin folder [default: NITRA_FFMPEG_DIR].
    [string] $FfmpegDir = $env:NITRA_FFMPEG_DIR,

    # Output folder [default: <clip folder>\<clip name>-quality-<yyyy-MM-dd-HHmm>].
    [string] $OutDir,
    [switch] $KeepRaw
)

$ErrorActionPreference = 'Continue'
$Encoders = @($Encoders | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim().ToLowerInvariant() } | Where-Object { $_ })

if (-not $FfmpegDir -or -not (Test-Path -LiteralPath (Join-Path $FfmpegDir 'ffmpeg.exe'))) {
    Write-Host 'ffmpeg not found: pass -FfmpegDir <folder with ffmpeg.exe> or set NITRA_FFMPEG_DIR.' -ForegroundColor Red
    exit 2
}
$ffmpeg = Join-Path $FfmpegDir 'ffmpeg.exe'

if ($Record) {
    if (Test-Path -LiteralPath $Clip) {
        Write-Host "Clip already exists: $Clip" -ForegroundColor Red
        exit 2
    }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Clip) | Out-Null
    Write-Host "Recording $RecordSeconds s of monitor $Monitor to $Clip (lossless RGB)..." -ForegroundColor Cyan
    & $ffmpeg -hide_banner -loglevel warning -stats -f lavfi `
        -i "gfxcapture=monitor_idx=${Monitor}:max_framerate=${Fps}:capture_cursor=0" -t $RecordSeconds `
        -vf 'hwdownload,format=bgra' -fps_mode cfr -r $Fps -c:v libx264rgb -qp 0 -preset ultrafast -y $Clip
    if ($LASTEXITCODE -ne 0) { Write-Host 'ffmpeg recording failed' -ForegroundColor Red; exit $LASTEXITCODE }
    Write-Host "Recorded: $Clip ($([math]::Round((Get-Item -LiteralPath $Clip).Length / 1MB)) MiB)"
    exit 0
}

if (-not (Test-Path -LiteralPath $Clip -PathType Leaf)) {
    Write-Host "Clip not found: $Clip" -ForegroundColor Red
    exit 2
}
$Clip = (Resolve-Path -LiteralPath $Clip).Path
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) {
    $OutDir = Join-Path (Split-Path -Parent $Clip) ('{0}-quality-{1:yyyy-MM-dd-HHmm}' -f [IO.Path]::GetFileNameWithoutExtension($Clip), (Get-Date))
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

$bench = Join-Path $repoRoot 'target\release\bench.exe'
Push-Location $repoRoot
try {
    & cargo build --release -p bench
    if ($LASTEXITCODE -ne 0) { Write-Host 'cargo build failed' -ForegroundColor Red; exit $LASTEXITCODE }
}
finally {
    Pop-Location
}

$sizes = @(@{ Name = '1080p'; Width = 1920; Height = 1080; Kbps = $Kbps })
if ($Kbps720.Count -gt 0) { $sizes += @{ Name = '720p'; Width = 1280; Height = 720; Kbps = $Kbps720 } }

$results = @()
foreach ($size in $sizes) {
    $reference = Join-Path $OutDir "reference-$($size.Name).nv12"
    Write-Host "Reference $($size.Name): $reference" -ForegroundColor Cyan
    # BT.709 limited range, the converter's output convention.
    & $ffmpeg -hide_banner -loglevel error -y -i $Clip `
        -vf "fps=$Fps,scale=$($size.Width):$($size.Height):flags=bicubic:out_color_matrix=bt709:out_range=tv,format=nv12" `
        -f rawvideo $reference
    if ($LASTEXITCODE -ne 0) { Write-Host 'reference conversion failed' -ForegroundColor Red; exit 1 }
    $frameBytes = $size.Width * $size.Height * 3 / 2
    $frames = (Get-Item -LiteralPath $reference).Length / $frameBytes
    $seconds = [math]::Ceiling($frames / $Fps) + 5

    foreach ($rate in $size.Kbps) {
        foreach ($encoder in $Encoders) {
            $name = '{0}-{1}k-{2}' -f $size.Name, $rate, $encoder
            $stream = Join-Path $OutDir "$name.h264"
            $summary = Join-Path $OutDir "$name.json"
            Write-Host "  $name" -ForegroundColor Cyan
            & $bench encode --encoder $encoder --source raw --file $reference --width $size.Width --height $size.Height `
                --fps $Fps --seconds $seconds --kbps $rate --output $stream --summary-json $summary | Out-File (Join-Path $OutDir "$name.log") -Encoding utf8
            if ($LASTEXITCODE -ne 0) { Write-Host "    bench encode failed; see $name.log" -ForegroundColor Red; continue }

            # Decode to raw NV12 with the range untouched, then compare raw to raw.
            $decoded = Join-Path $OutDir "$name.nv12"
            & $ffmpeg -hide_banner -loglevel error -y -i $stream -vf 'scale=in_range=tv:out_range=tv' -pix_fmt nv12 -f rawvideo $decoded
            $rawInput = @('-f', 'rawvideo', '-pix_fmt', 'nv12', '-s', "$($size.Width)x$($size.Height)", '-r', $Fps)
            $graph = '[0:v]format=yuv420p,split=3[d0][d1][d2];[1:v]format=yuv420p,split=3[r0][r1][r2];' +
                "[d0][r0]psnr;[d1][r1]ssim;[d2][r2]libvmaf=n_threads=8:log_fmt=json:log_path=vmaf-$name.json"
            # libvmaf writes its per-frame log relative to the working folder
            # (a Windows path would clash with the filter syntax's colons).
            Push-Location -LiteralPath $OutDir
            $metrics = & $ffmpeg -hide_banner -nostats @rawInput -i $decoded @rawInput -i $reference -lavfi $graph -f null - 2>&1 |
                ForEach-Object { "$_" }
            Pop-Location
            $vmafFrames = @((Get-Content -LiteralPath (Join-Path $OutDir "vmaf-$name.json") -Raw | ConvertFrom-Json).frames | ForEach-Object { [double] $_.metrics.vmaf } | Sort-Object)
            $vmafP5 = if ($vmafFrames.Count) { $vmafFrames[[math]::Floor(0.05 * ($vmafFrames.Count - 1))] } else { $null }
            $psnr = ($metrics | Select-String 'PSNR y:([\d.]+) u:([\d.]+) v:([\d.]+) average:([\d.]+)').Matches
            $ssim = ($metrics | Select-String 'SSIM Y:([\d.]+).*All:([\d.]+)').Matches
            $vmaf = ($metrics | Select-String 'VMAF score: ([\d.]+)').Matches
            $json = Get-Content -LiteralPath $summary -Raw | ConvertFrom-Json
            $decodedFrames = (Get-Item -LiteralPath $decoded).Length / $frameBytes
            $row = [ordered]@{
                size            = $size.Name
                kbps            = $rate
                encoder         = $encoder
                frames          = $frames
                decoded_frames  = $decodedFrames
                achieved_mbps   = [math]::Round($json.achieved_bitrate_bps / 1e6, 3)
                psnr_y          = if ($psnr) { [double] $psnr[0].Groups[1].Value } else { $null }
                psnr_avg        = if ($psnr) { [double] $psnr[0].Groups[4].Value } else { $null }
                ssim_y          = if ($ssim) { [double] $ssim[0].Groups[1].Value } else { $null }
                ssim_all        = if ($ssim) { [double] $ssim[0].Groups[2].Value } else { $null }
                vmaf            = if ($vmaf) { [double] $vmaf[0].Groups[1].Value } else { $null }
                vmaf_p5         = $vmafP5
                latency_p50_ms  = $json.latency_ms.p50
                busy_retries    = $json.busy_retries
            }
            if ($decodedFrames -ne $frames) {
                Write-Host "    decoded $decodedFrames of $frames frames: metrics are not aligned" -ForegroundColor Red
            }
            $results += [pscustomobject] $row
            if (-not $KeepRaw) { Remove-Item -LiteralPath $decoded }
        }
    }
    if (-not $KeepRaw) { Remove-Item -LiteralPath $reference }
}

$results | Export-Csv -LiteralPath (Join-Path $OutDir 'quality.csv') -NoTypeInformation -Encoding utf8
[ordered]@{
    clip     = $Clip
    clip_sha256 = (Get-FileHash -LiteralPath $Clip -Algorithm SHA256).Hash
    fps      = $Fps
    method   = 'raw NV12 reference (BT.709, limited range) vs decoded output (range untouched); ffmpeg psnr, ssim, libvmaf (default model)'
    results  = $results
} | ConvertTo-Json -Depth 4 | Out-File (Join-Path $OutDir 'quality.json') -Encoding utf8
$results | Format-Table size, kbps, encoder, achieved_mbps, psnr_y, ssim_all, vmaf, vmaf_p5, latency_p50_ms, decoded_frames -AutoSize
Write-Host "Results: $OutDir"
exit 0
