param([string]$Codec = 'h264', [int]$Seconds = 30)
# Measures rbuf's CPU time, memory and the GPU encoder's load while it buffers a moving test pattern.
# Usage: powershell -File scripts\perf.ps1 -Codec hevc -Seconds 30   (needs ffplay and nvidia-smi on PATH)
$probe = Join-Path $PSScriptRoot '..\target\perf'
New-Item -ItemType Directory -Force $probe | Out-Null
$rbuf = Join-Path $PSScriptRoot '..\target\release\rbuf.exe'
$play = Start-Process ffplay -ArgumentList '-v','error','-f','lavfi','-i','testsrc2=s=1920x1080:r=60','-fs','-an' -PassThru
Start-Sleep 2
$p = Start-Process $rbuf -ArgumentList '-w','screen','-r','60','-k',$Codec,'-a','default_output','-o',"$probe\clips" -PassThru -WindowStyle Hidden
Start-Sleep 5   # warm up, and the buffer fills
$cpu0 = $p.TotalProcessorTime.TotalSeconds
$t0 = Get-Date
$smi = Start-Process nvidia-smi -ArgumentList '--query-gpu=utilization.gpu,utilization.encoder','--format=csv,noheader,nounits','-lms','500' -RedirectStandardOutput "$probe\smi.txt" -PassThru -WindowStyle Hidden
Start-Sleep $Seconds
$p.Refresh()
$cpu1 = $p.TotalProcessorTime.TotalSeconds
$t1 = Get-Date
$ws = $p.WorkingSet64
$priv = $p.PrivateMemorySize64
Stop-Process $smi.Id
& $rbuf stop
Start-Sleep 2
Stop-Process $play.Id -ErrorAction SilentlyContinue
$wall = ($t1 - $t0).TotalSeconds
$enc = Get-Content "$probe\smi.txt" | ForEach-Object { [int]($_.Split(',')[1].Trim()) } | Measure-Object -Average -Maximum
$gpu = Get-Content "$probe\smi.txt" | ForEach-Object { [int]($_.Split(',')[0].Trim()) } | Measure-Object -Average
"codec {0}: rbuf CPU {1:N2}% of one logical CPU ({2:N2} s in {3:N1} s); working set {4:N0} MB, private {5:N0} MB; NVENC {6:N1}% average, {7}% max; GPU {8:N1}% average" -f $Codec, (100 * ($cpu1 - $cpu0) / $wall), ($cpu1 - $cpu0), $wall, ($ws / 1MB), ($priv / 1MB), $enc.Average, $enc.Maximum, $gpu.Average
