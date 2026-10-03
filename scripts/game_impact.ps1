param([int]$Seconds = 10, [int]$Rounds = 3)
# The frame rate of examples/game.rs (an uncapped full-screen renderer) alone and while rbuf
# records the screen with each capture method. Build first: cargo build --release --examples
$root = Join-Path $PSScriptRoot '..\target\release'
$out = Join-Path $PSScriptRoot '..\target\impact'
New-Item -ItemType Directory -Force $out | Out-Null
$results = @{}
for ($r = 0; $r -lt $Rounds; $r++) {
    foreach ($c in 'none', 'nvfbc', 'wgc', 'dxgi') {
        $p = $null
        if ($c -ne 'none') {
            $p = Start-Process "$root\rbuf.exe" -ArgumentList '-w','screen','-capture',$c,'-o',"$out\$c.mp4",'-t',"$($Seconds + 4)" -PassThru -WindowStyle Hidden
            Start-Sleep 1
        }
        $fps = [double](((& "$root\examples\game.exe" $Seconds) -split ' ')[0])
        if ($p) { $p.WaitForExit() }
        $results[$c] += @($fps)
        Start-Sleep 1
    }
}
foreach ($c in 'none', 'nvfbc', 'wgc', 'dxgi') {
    $m = $results[$c] | Measure-Object -Average -Minimum -Maximum
    "{0,-6} {1,8:N0} fps average ({2:N0} to {3:N0})" -f $c, $m.Average, $m.Minimum, $m.Maximum
}
