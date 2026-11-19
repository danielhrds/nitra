#Requires -Version 5.1
<#
.SYNOPSIS
    Cold start of both UI prototypes: run once right after booting Windows.

.DESCRIPTION
    Starts each prototype once (list screen, quits after 3 s) and records the
    time from process creation to its first drawn frame, plus the order and
    the boot time, in D:\Projects\live\lab\ui-spike\startup-cold.csv. The
    order alternates on every boot (the first app also pays for loading the
    shared Direct3D / driver / Media Foundation DLLs). Run it only once per
    boot: later runs are no longer cold.

    Keep this file ASCII-only (PowerShell 5.1).
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$spikes = $PSScriptRoot
$out = 'D:\Projects\live\lab\ui-spike'
$csv = Join-Path $out 'startup-cold.csv'
New-Item -ItemType Directory -Force $out | Out-Null

# The latest boot, from the Kernel-Boot event (id 27), which also records
# Fast Startup boots ("Shut down" with Fast Startup on hibernates the kernel
# and drivers; LastBootUpTime then keeps the previous full boot). Its type:
# 0x0 full boot (use "Restart"), 0x1 Fast Startup.
$event = Get-WinEvent -FilterHashtable @{ LogName = 'System'; ProviderName = 'Microsoft-Windows-Kernel-Boot'; Id = 27 } -MaxEvents 1
$boot = $event.TimeCreated
$bootType = if ($event.Message -match '0x[0-9a-fA-F]+') { $Matches[0] } else { '?' }
$bootText = $boot.ToString('yyyy-MM-dd HH:mm:ss')
$uptime = (Get-Date) - $boot
if ($bootType -ne '0x0') {
    Write-Warning "This boot was not a full boot (type $bootType, e.g. Fast Startup after 'Shut down'). Use 'Restart' for a cold start; measuring anyway, recorded with its type."
}
if (Test-Path $csv) {
    $rows = @(Import-Csv $csv)
    if ($rows | Where-Object { $_.boot -eq $bootText }) {
        throw "Already measured on this boot ($bootText). Reboot first."
    }
    $boots = @($rows | Select-Object -ExpandProperty boot -Unique).Count
} else {
    $boots = 0
}

$apps = @(
    @{ Name = 'imgui'; Exe = Join-Path $spikes 'target\release\ui-spike-imgui.exe' },
    @{ Name = 'gpui';  Exe = Join-Path $spikes 'target\release\ui-spike-gpui.exe' }
)
if ($boots % 2 -eq 1) { [array]::Reverse($apps) }

$position = 0
foreach ($app in $apps) {
    $position++
    $output = & $app.Exe --seconds 3 2>&1 | Out-String
    $match = [regex]::Match($output, 'first frame ([0-9.]+) ms')
    $ms = if ($match.Success) { $match.Groups[1].Value } else { 'NA' }
    [pscustomobject]@{
        boot       = $bootText
        boot_type  = $bootType
        uptime_s   = [int]$uptime.TotalSeconds
        app        = $app.Name
        position   = $position
        first_frame_ms = $ms
    } | Export-Csv $csv -Append -NoTypeInformation
    Write-Host ("{0} (opened {1}): first frame {2} ms" -f $app.Name, $position, $ms)
    Start-Sleep -Seconds 1
}
Write-Host "Saved to $csv (boot $($boots + 1))."
