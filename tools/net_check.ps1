# Starts the app and lists every network connection that it and its WebView2
# processes hold during -Seconds, sampled twice a second (AC-16). Without -AppArgs the app waits on
# the first screen; with two files and --key it compares them at once, and
# -ExpectRecent then checks that the comparison really ran (the app records
# it in the list of recent comparisons).
# Connections to loopback addresses are internal (the screens talk to the
# app over its own channel); any other remote address fails the check.
#
#   pwsh tools/net_check.ps1 -Exe out/Recolign.exe [-Seconds 20]
#   pwsh tools/net_check.ps1 -Exe out/Recolign.exe -AppArgs A.csv,B.csv,--key,社員番号 -ExpectRecent B.csv
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [int]$Seconds = 20,
  [string[]]$AppArgs = @(),
  [string]$ExpectRecent = ''
)
$ErrorActionPreference = 'Stop'
# Names already in the DNS cache, to list the ones looked up during the check.
$dnsBefore = @{}
Get-DnsClientCache -ErrorAction SilentlyContinue | ForEach-Object { $dnsBefore["$($_.Entry)|$($_.Data)"] = $true }
$app = if ($AppArgs.Count) {
  Start-Process -FilePath $Exe -ArgumentList ($AppArgs | ForEach-Object { "`"$_`"" }) -PassThru
} else {
  Start-Process -FilePath $Exe -PassThru
}

# The app and every process below it (WebView2 browser, renderer, GPU, utility).
function Get-Tree([int]$Root) {
  $all = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name
  $ids = @($Root)
  do {
    $more = $all | Where-Object { $ids -contains $_.ParentProcessId -and $ids -notcontains $_.ProcessId } | ForEach-Object ProcessId
    $ids += $more
  } while ($more)
  return ,@($all | Where-Object { $ids -contains $_.ProcessId })
}

# Sampled every half second for the whole time, so a connection that opens
# and closes in between is still seen (the last sample alone would miss it).
$seen = @{}; $udpSeen = @{}; $tree = @()
$sw = [Diagnostics.Stopwatch]::StartNew()
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
  if ($app.HasExited) { throw "The app exited during the check (code $($app.ExitCode))." }
  $tree = Get-Tree $app.Id
  $ids = $tree | ForEach-Object ProcessId
  foreach ($c in @(Get-NetTCPConnection -ErrorAction SilentlyContinue | Where-Object { $ids -contains $_.OwningProcess })) {
    $seen["$($c.OwningProcess)|$($c.LocalAddress)|$($c.LocalPort)|$($c.RemoteAddress)|$($c.RemotePort)"] = $c
  }
  foreach ($u in @(Get-NetUDPEndpoint -ErrorAction SilentlyContinue | Where-Object { $ids -contains $_.OwningProcess })) {
    $udpSeen["$($u.OwningProcess)|$($u.LocalAddress)|$($u.LocalPort)"] = $u
  }
  Start-Sleep -Milliseconds 500
}
$tree | Format-Table ProcessId, ParentProcessId, Name -AutoSize | Out-String | Write-Host
# The arguments the app gave the WebView2 browser process.
$browser = Get-CimInstance Win32_Process -Filter "ParentProcessId = $($app.Id) AND Name = 'msedgewebview2.exe'" | Select-Object -First 1
if ($browser) {
  $flags = [regex]::Matches($browser.CommandLine, '--(disable|enable|no)-[\w-]+(=\S+)?') | ForEach-Object Value
  Write-Host "WebView2 started with: $($flags -join ' ')"
}

$loop = { param($a) $a -eq '0.0.0.0' -or $a -eq '::' -or $a -like '127.*' -or $a -like '::ffff:127.*' -or $a -eq '::1' }
$tcp = @($seen.Values)
$udp = @($udpSeen.Values)
Write-Host "TCP connections: $($tcp.Count)"
$tcp | Format-Table OwningProcess, LocalAddress, LocalPort, RemoteAddress, RemotePort, State -AutoSize | Out-String | Write-Host
Write-Host "UDP endpoints: $($udp.Count)"
$udp | Format-Table OwningProcess, LocalAddress, LocalPort -AutoSize | Out-String | Write-Host

$outside = @($tcp | Where-Object { -not (& $loop $_.RemoteAddress) })
Stop-Process -Id $app.Id -Force
if ($ExpectRecent) {
  $history = Join-Path $env:APPDATA 'io.github.okakanatto.recolign\history.json'
  if (-not (Test-Path $history) -or -not ((Get-Content -Raw -Encoding utf8 $history) -like "*$ExpectRecent*")) {
    throw "The comparison did not start: $ExpectRecent is not in the recent comparisons ($history)."
  }
  Write-Host "The comparison ran ($ExpectRecent is in the recent comparisons)."
}
if ($outside.Count) {
  $outside | Format-Table OwningProcess, RemoteAddress, RemotePort, State -AutoSize | Out-String | Write-Host
  # Which names the addresses came from (A and CNAME records in the cache).
  $cache = @(Get-DnsClientCache -ErrorAction SilentlyContinue)
  $new = @($cache | Where-Object { -not $dnsBefore.ContainsKey("$($_.Entry)|$($_.Data)") })
  $remote = @($outside | ForEach-Object RemoteAddress)
  Write-Host 'Names for these addresses, and names looked up during the check:'
  @($cache | Where-Object { $remote -contains $_.Data }) + $new | Sort-Object Entry, Data -Unique |
    Format-Table Entry, Type, Data -AutoSize | Out-String -Width 200 | Write-Host
  throw "The app or its WebView2 processes connected outside this machine ($($outside.Count) connections)."
}
Write-Host 'No connections outside this machine.'
