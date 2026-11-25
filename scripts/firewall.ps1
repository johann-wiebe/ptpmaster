<#
.SYNOPSIS
    Legt die Windows-Firewall-Regeln für ptpmaster (PTP, UDP 319/320) an oder entfernt sie.

.DESCRIPTION
    PTP über UDP/IPv4 nutzt Port 319 (Event: Sync, Delay_Req) und Port 320 (General:
    Announce, Follow_Up, Delay_Resp). Ohne eingehende Regeln verwirft die Firewall die
    Delay_Req der Slaves (und für "ptpmaster probe" die Nachrichten des Masters).

    Das Skript ist idempotent: vorhandene Regeln der Gruppe werden vor dem Anlegen entfernt.

.PARAMETER Remove
    Regeln entfernen statt anlegen.

.PARAMETER NetworkProfile
    Firewall-Profile, für die die Regeln gelten (Standard: Domain, Private).
    "Public" nur hinzufügen, wenn das LAN in Windows als "Öffentlich" eingestuft ist.

.PARAMETER Program
    Optional: Regeln auf diese exe beschränken (z. B. C:\ptpmaster\ptpmaster.exe).

.EXAMPLE
    .\firewall.ps1
    .\firewall.ps1 -NetworkProfile Domain,Private,Public
    .\firewall.ps1 -Program C:\ptpmaster\ptpmaster.exe
    .\firewall.ps1 -Remove
#>
[CmdletBinding()]
param(
    [switch]$Remove,
    [ValidateSet('Domain', 'Private', 'Public', 'Any')]
    [string[]]$NetworkProfile = @('Domain', 'Private'),
    [string]$Program
)

$ErrorActionPreference = 'Stop'
$Group = 'ptpmaster (PTP IEEE 1588)'

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error 'Bitte PowerShell als Administrator starten.'
    exit 1
}

$existing = Get-NetFirewallRule -Group $Group -ErrorAction SilentlyContinue
if ($existing) {
    $existing | Remove-NetFirewallRule
    Write-Host "Vorhandene Regeln der Gruppe '$Group' entfernt ($(@($existing).Count))."
}
if ($Remove) {
    if (-not $existing) { Write-Host "Keine Regeln der Gruppe '$Group' vorhanden." }
    exit 0
}

$rules = @(
    @{ Name = 'ptpmaster-event-in';    DisplayName = 'PTP Event (UDP 319) eingehend';   Direction = 'Inbound';  Port = 319 },
    @{ Name = 'ptpmaster-general-in';  DisplayName = 'PTP General (UDP 320) eingehend'; Direction = 'Inbound';  Port = 320 },
    @{ Name = 'ptpmaster-event-out';   DisplayName = 'PTP Event (UDP 319) ausgehend';   Direction = 'Outbound'; Port = 319 },
    @{ Name = 'ptpmaster-general-out'; DisplayName = 'PTP General (UDP 320) ausgehend'; Direction = 'Outbound'; Port = 320 }
)

foreach ($r in $rules) {
    $params = @{
        Name        = $r.Name
        DisplayName = $r.DisplayName
        Group       = $Group
        Direction   = $r.Direction
        Protocol    = 'UDP'
        Action      = 'Allow'
        Profile     = $NetworkProfile
        Description = 'Precision Time Protocol (IEEE 1588-2008) für ptpmaster'
    }
    if ($r.Direction -eq 'Inbound') { $params.LocalPort = $r.Port } else { $params.RemotePort = $r.Port }
    if ($Program) { $params.Program = (Resolve-Path $Program).Path }
    New-NetFirewallRule @params | Out-Null
    Write-Host "Angelegt: $($r.DisplayName)"
}

Write-Host ''
Write-Host "Fertig. Profile: $($NetworkProfile -join ', '). Entfernen mit: .\firewall.ps1 -Remove"
Get-NetFirewallRule -Group $Group | Format-Table DisplayName, Enabled, Profile, Direction, Action -AutoSize
