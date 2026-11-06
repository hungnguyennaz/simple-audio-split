param(
    [string]$Exe = 'target/release/sas-cli.exe',
    [string]$TargetDevice = '0',
    [int]$Seconds = 1800,
    [double[]]$Margins = @(0, 0.5, 1),
    [string]$OutDirectory = 'target/perf',
    [switch]$Verify
)
$ErrorActionPreference = 'Stop'
if ($Seconds -lt 10) { throw 'Use at least 10 seconds for meaningful process measurements.' }
$executable = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Path $OutDirectory -Force | Out-Null
$output = (Resolve-Path -LiteralPath $OutDirectory).Path
# Save paths before stopping: Process.Path can become unavailable once the process exits.
$previous = @(Get-Process -Name SimpleAudioSplit -ErrorAction SilentlyContinue | Where-Object Path | ForEach-Object { [PSCustomObject]@{ Id=$_.Id; Path=$_.Path } })
$results = [Collections.Generic.List[object]]::new()
$process = $null
try {
    foreach ($app in $previous) { Stop-Process -Id $app.Id -ErrorAction Stop }
    foreach ($margin in $Margins) {
        $value = $margin.ToString([Globalization.CultureInfo]::InvariantCulture)
        $name = "margin-$value-$(Get-Date -Format yyyyMMdd-HHmmss)"
        $log = Join-Path $output "$name.log"
        $err = Join-Path $output "$name.err"
        $process = Start-Process -FilePath $executable -ArgumentList @('run','--target',$TargetDevice,'--seconds',$Seconds,'--margin',$value) -RedirectStandardOutput $log -RedirectStandardError $err -WindowStyle Hidden -PassThru
        $null = $process.Handle # Retain the handle so Windows PowerShell can read ExitCode after exit.
        $watch = [Diagnostics.Stopwatch]::StartNew()
        $samples = [Collections.Generic.List[object]]::new()
        $failed = $false
        $lastProgress = -60
        while (-not $process.HasExited) {
            try {
                $process.Refresh()
                $samples.Add([PSCustomObject]@{ Seconds=$watch.Elapsed.TotalSeconds; CpuMs=$process.TotalProcessorTime.TotalMilliseconds; Private=$process.PrivateMemorySize64; WorkingSet=$process.WorkingSet64 })
            } catch { if (-not $process.HasExited) { throw } }
            $tail = @(Get-Content -LiteralPath $log -Tail 8 -ErrorAction SilentlyContinue)
            if ($tail -match 'underruns [1-9][0-9]*|resyncs [1-9][0-9]*') {
                $failed = $true
                Stop-Process -Id $process.Id
                break
            }
            if ($watch.Elapsed.TotalSeconds - $lastProgress -ge 60) {
                Write-Output "Margin $value ms: $([int]$watch.Elapsed.TotalSeconds)/$Seconds seconds"
                $lastProgress = $watch.Elapsed.TotalSeconds
            }
            Start-Sleep -Seconds 5
        }
        $process.WaitForExit()
        $exitCode = $process.ExitCode
        $stable = @($samples | Where-Object Seconds -GE 3)
        $cpu = $null
        if ($stable.Count -gt 1) { $cpu = ($stable[-1].CpuMs-$stable[0].CpuMs)/($stable[-1].Seconds-$stable[0].Seconds) }
        $passed = -not $failed -and $exitCode -eq 0 -and $watch.Elapsed.TotalSeconds -ge $Seconds
        $verificationExit = $null
        if ($passed -and $Verify) {
            $verification = Start-Process -FilePath $executable -ArgumentList @('verify','--source','default','--target',$TargetDevice,'--seconds','8','--margin',$value) -RedirectStandardOutput (Join-Path $output "$name-verify.log") -RedirectStandardError (Join-Path $output "$name-verify.err") -WindowStyle Hidden -PassThru
            $null = $verification.Handle
            $verification.WaitForExit()
            $verificationExit = $verification.ExitCode
            $passed = $verificationExit -eq 0
        }
        $result = [PSCustomObject]@{
            MarginMs=$margin; RequestedSeconds=$Seconds; ElapsedSeconds=$watch.Elapsed.TotalSeconds
            Passed=$passed; ExitCode=$exitCode; VerificationExitCode=$verificationExit; CpuMsPerSecond=$cpu
            PrivateBytes=($stable | Measure-Object Private -Average).Average
            WorkingSetBytes=($stable | Measure-Object WorkingSet -Average).Average
            PeakPrivateBytes=($samples | Measure-Object Private -Maximum).Maximum
            Log=$log; ErrorLog=$err
        }
        $results.Add($result)
        $results | ConvertTo-Json -Depth 4 | Set-Content -Encoding UTF8 (Join-Path $output 'soak-results.json')
        $result | ConvertTo-Json -Compress | Write-Output
        $process = $null
    }
} finally {
    if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id }
    foreach ($app in $previous) { Start-Process -FilePath $app.Path -ArgumentList '--tray' -WindowStyle Hidden }
}
