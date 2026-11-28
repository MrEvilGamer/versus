param(
  [string]$Log = 'C:\Games\GTA VI\versus\bench-logs\texture-accounting-info.err.log',
  [int]$IntervalSeconds = 3
)
$ErrorActionPreference = 'Stop'
while ($true) {
  Write-Host "`n--- refresh ---"
  $proc = Get-Process versus -ErrorAction SilentlyContinue | Select-Object -First 1
  Write-Host "VERSUS LIVE TELEMETRY  $(Get-Date -Format 'HH:mm:ss')" -ForegroundColor Cyan
  if($proc){ Write-Host "Process: RUNNING (PID $($proc.Id))" } else { Write-Host "Process: NOT RUNNING" }
  if (Test-Path -LiteralPath $Log) {
    $lines = Get-Content -LiteralPath $Log -Tail 300
    $last = $lines | Where-Object { $_ -match 'fps, .*draws/frame' } | Select-Object -Last 1
    $queue = $lines | Where-Object { $_ -match 'textures picked by index, the queue:' } | Select-Object -Last 1
    $card = $lines | Where-Object { $_ -match 'card accounting: room' } | Select-Object -Last 1
    if($last){ Write-Host "`nFRAME:`n$last" }
    if($queue){ Write-Host "`nTEXTURE QUEUE:`n$queue" }
    if($card){ Write-Host "`nVRAM/CARD:`n$card" }
    $errors = $lines | Where-Object { $_ -match 'ERROR|device removed|panic|fatal' } | Select-Object -Last 5
    if($errors){ Write-Host "`nERRORS:" -ForegroundColor Yellow; $errors }
  } else { Write-Host "Log not found: $Log" -ForegroundColor Yellow }
  Write-Host "`nRead-only monitor. Press Ctrl+C to exit."
  Start-Sleep -Seconds $IntervalSeconds
}
