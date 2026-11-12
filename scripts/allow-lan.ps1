# Optional one-time setup from an elevated PowerShell on the SERVER computer.
# The service installer already adds equivalent rules for the installed copy on
# the Private profile. Use this script for Public/Domain networks, skipped rules,
# or a portable server.exe. It defaults to the installed copy.
#Requires -RunAsAdministrator
param([string]$ServerPath = (Join-Path $env:ProgramFiles 'Screener\server.exe'))
$ErrorActionPreference = 'Stop'
$serverExe = (Resolve-Path -LiteralPath $ServerPath).Path
New-NetFirewallRule -DisplayName 'Screener LAN video (UDP) and control' -Direction Inbound -Action Allow `
    -Program $serverExe -Protocol UDP -LocalPort 47990 -Profile Private -RemoteAddress LocalSubnet
New-NetFirewallRule -DisplayName 'Screener LAN video and input' -Direction Inbound -Action Allow `
    -Program $serverExe -Protocol TCP -LocalPort 47990,47991,47993 -Profile Private -RemoteAddress LocalSubnet
New-NetFirewallRule -DisplayName 'Screener LAN discovery' -Direction Inbound -Action Allow `
    -Program $serverExe -Protocol UDP -LocalPort 47992 -Profile Private -RemoteAddress LocalSubnet
