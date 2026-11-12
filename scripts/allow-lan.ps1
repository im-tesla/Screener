# Optional one-time setup from an elevated PowerShell on the SERVER computer.
# The application itself neither elevates nor changes firewall settings.
# When copied into dist, server.exe is beside this script.
#Requires -RunAsAdministrator
param([string]$ServerPath = (Join-Path $PSScriptRoot 'server.exe'))
$ErrorActionPreference = 'Stop'
$serverExe = (Resolve-Path -LiteralPath $ServerPath).Path
New-NetFirewallRule -DisplayName 'Screener LAN video and input' -Direction Inbound -Action Allow `
    -Program $serverExe -Protocol TCP -LocalPort 47990,47991,47993 -Profile Private -RemoteAddress LocalSubnet
New-NetFirewallRule -DisplayName 'Screener LAN discovery' -Direction Inbound -Action Allow `
    -Program $serverExe -Protocol UDP -LocalPort 47992 -Profile Private -RemoteAddress LocalSubnet
