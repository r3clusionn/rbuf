"""Summarises bench.py logs (one JSON object per line): mean and range per condition."""
import collections
import json
import statistics
import sys

rows = []
for path in sys.argv[1:]:
    for line in open(path, encoding='utf-8', errors='replace'):
        line = line.strip()
        if line.startswith('{'):
            rows.append(json.loads(line))

by = collections.defaultdict(list)
for r in rows:
    by[(r['load'], r['cond'])].append(r)

order = ['none', 'rbuf-nvfbc', 'rbuf-process', 'rbuf-wgc', 'obs-display', 'obs-game', 'nvapp-idle', 'shadowplay']
short = {'Hardware: Independent Flip': 'indep.', 'Hardware: Legacy Flip': 'legacy', 'Composed: Flip': 'composed'}
for load in sorted({r['load'] for r in rows}):
    base = statistics.mean(r['fps'] for r in by[(load, 'none')]) if by[(load, 'none')] else None
    nv = statistics.mean(r['fps'] for r in by[(load, 'nvapp-idle')]) if by[(load, 'nvapp-idle')] else None
    print(f'== {load}')
    print(f"{'condition':13}{'n':>2}{'fps':>9}{'range':>13}{'vs base':>8}{'1%low':>7}{'mode':>10}{'lat ms':>7}{'p95':>6}"
          f"{'recCPU':>7}{'dwm':>6}{'sys':>6}{'recMB':>6}{'NVENC':>6}{'outfps':>7}{'Mb/s':>6}")
    for c in order:
        rs = by.get((load, c))
        if not rs:
            continue
        m = lambda k: statistics.mean((r.get(k) or 0) for r in rs)
        fps = [r['fps'] for r in rs]
        b = nv if c in ('nvapp-idle', 'shadowplay') and nv else base
        rel = f'{100 * m("fps") / b:6.1f}%' if b else ''
        mode = collections.Counter(short.get(r.get('mode'), r.get('mode') or '?') for r in rs).most_common(1)[0][0]
        print(f'{c:13}{len(rs):>2}{m("fps"):9.1f}{min(fps):6.0f}-{max(fps):<6.0f}{rel:>8}{m("low"):7.0f}{mode:>10}'
              f'{m("latency_ms"):7.1f}{m("latency_p95_ms"):6.1f}{m("rec_cpu"):6.1f}%{m("dwm_cpu"):5.1f}%{m("sys_cpu"):5.0f}%'
              f'{m("rec_mem_mb"):6.0f}{m("nvenc"):6.1f}{m("out_fps"):7.2f}{m("out_mbps"):6.2f}')
    print('(shadowplay and nvapp-idle are relative to nvapp-idle: the NVIDIA App running, not recording)')
