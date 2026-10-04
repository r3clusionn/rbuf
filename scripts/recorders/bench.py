"""Recorder benchmark: a full-screen game stand-in (examples/game.rs) runs while each recorder
records the screen, all at ShadowPlay's "High" settings for 1080p60: H.264 on NVENC, VBR 16 Mbit/s
(32 max), 60 fps, no B-frames, desktop audio as one AAC track.

Per run: the game's steady-state frame rate and 1% low (after `skip` seconds), its presentation
mode and display latency (PresentMon), CPU time of the recorder's processes, of dwm.exe and of the
whole machine over the same window, GPU and NVENC load (nvidia-smi), and the recording's real
frame rate and bitrate. Conditions run in a rotated order each round; the ones that need the
NVIDIA App (its idle cost and ShadowPlay) run as a block with its service started, the others with
it stopped, so its in-game overlay hook does not touch them.

    python bench.py LOAD ROUNDS [conditions...]
    LOAD: game.exe arguments joined by '+': heavy:200 (about 1,200 fps here), heavy:1000 (about
    245 fps), heavy:200+exclusive (exclusive full screen; Legacy Flip once full-screen
    optimisations are off for game.exe), light

BENCH_FPS=120 records at 120 fps instead of 60, at ShadowPlay's "High" bitrate for that rate (27
Mbit/s, 54 max). ShadowPlay's own frame rate setting is switched to the same rate for the run and
put back afterwards.

Needs: an administrator prompt (it stops and starts NvContainerLocalSystem), `cargo build --release
--examples`, `pip install psutil websocket-client`, nvidia-smi and ffprobe on PATH, PresentMon 2.x
(`PRESENTMON=path`), OBS Studio 30+ (`OBS_STUDIO=folder`; run in portable mode with its own
configuration, see obs_setup.py), and the NVIDIA App with ShadowPlay set to the High quality
preset (H.264, VBR 16 Mbit/s at 1080p60 and 27 Mbit/s at 1080p120, read from its CaptureCore.log),
manual recording on Alt+F9.
"""
import json
import os
import subprocess
import sys
import threading
import time

import psutil

import obsctl
import spctl

HERE = os.path.dirname(os.path.abspath(__file__))
PROJ = os.path.abspath(os.path.join(HERE, '..', '..'))
GAME = os.path.join(PROJ, 'target', 'release', 'examples', 'game.exe')
RBUF = os.path.join(PROJ, 'target', 'release', 'rbuf.exe')
OUT = os.path.join(PROJ, 'target', 'recorders')
os.makedirs(OUT, exist_ok=True)
SECONDS, SKIP = 25, 8
FPS = int(os.environ.get('BENCH_FPS', '60'))
# ShadowPlay's "High" preset at 1080p (its CaptureCore.log): kbit/s by frame rate.
KBPS = {60: 16000, 120: 27000}[FPS]
XPERF = os.environ.get('XPERF', r'C:\Program Files (x86)\Windows Kits\10\Windows Performance Toolkit\xperf.exe')
TRACE = os.environ.get('BENCH_XPERF') == '1'

RECORDER_PROCS = {
    'rbuf': {'rbuf.exe'},
    'obs': {'obs64.exe', 'obs-ffmpeg-mux.exe'},
    'shadowplay': {'nvcontainer.exe', 'nvidia overlay.exe', 'nvsphelper64.exe'},
    'none': set(),
}


def procs_named(names):
    out = []
    for p in psutil.process_iter(['name']):
        if (p.info['name'] or '').lower() in names:
            out.append(p)
    return out


def cpu_of(ps):
    t = {}
    for p in ps:
        try:
            c = p.cpu_times()
            t[p.pid] = c.user + c.system
        except psutil.Error:
            pass
    return t


def mem_of(ps):
    m = 0
    for p in ps:
        try:
            m += p.memory_info().rss
        except psutil.Error:
            pass
    return m


class Smi:
    """nvidia-smi sampled every 200 ms, read back for a time window."""

    def __init__(self):
        self.rows = []
        self.p = subprocess.Popen(['nvidia-smi', '--query-gpu=utilization.gpu,utilization.encoder', '--format=csv,noheader,nounits',
                                   '-lms', '200'], stdout=subprocess.PIPE, text=True)
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            try:
                g, e = (int(x) for x in line.split(','))
                self.rows.append((time.time(), g, e))
            except ValueError:
                pass

    def window(self, t0, t1):
        r = [(g, e) for t, g, e in self.rows if t0 <= t <= t1]
        if not r:
            return (0, 0)
        return (sum(g for g, _ in r) / len(r), sum(e for _, e in r) / len(r))

    def stop(self):
        self.p.kill()


def probe(path):
    if not path or not os.path.exists(path):
        return {}
    r = subprocess.run(['ffprobe', '-v', 'error', '-select_streams', 'v', '-count_packets', '-show_entries',
                        'stream=codec_name,width,height,nb_read_packets:format=duration,size', '-of', 'json', path],
                       capture_output=True, text=True)
    j = json.loads(r.stdout or '{}')
    s = (j.get('streams') or [{}])[0]
    f = j.get('format', {})
    dur = float(f.get('duration', 0) or 0)
    n = int(s.get('nb_read_packets', 0) or 0)
    return {'codec': s.get('codec_name'), 'size': f"{s.get('width')}x{s.get('height')}", 'fps': n / dur if dur else 0,
            'mbps': int(f.get('size', 0)) * 8 / dur / 1e6 if dur else 0, 'duration': dur}


# ---- recorders ---------------------------------------------------------------------------------

class Rbuf:
    family = 'rbuf'

    def __init__(self, capture, target=('-w', 'screen'), env=None):
        self.capture, self.target, self.env = capture, list(target), env

    def prepare(self):
        pass

    def start(self, tag):
        self.path = os.path.join(OUT, f'{tag}.mp4')
        self.p = subprocess.Popen([RBUF] + self.target + ['-capture', self.capture, '-f', str(FPS), '-k', 'h264', '-bm', 'vbr', '-q', str(KBPS),
                                   '-gop', '1', '-a', 'default_output', '-o', self.path],
                                  stderr=subprocess.PIPE, text=True, env={**os.environ, **(self.env or {})})

    def stop(self):
        subprocess.run([RBUF, 'stop'], capture_output=True)
        try:
            self.log = self.p.communicate(timeout=20)[1]
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.log = 'killed'
        return self.path

    def cleanup(self):
        pass


class Obs:
    family = 'obs'

    def __init__(self, source):
        self.source = source

    def prepare(self):
        subprocess.run(['python', os.path.join(HERE, 'obs_setup.py'), str(KBPS), 'p4', 'vbr', str(FPS)], check=True, capture_output=True)
        self.proc = obsctl.launch()
        self.o = obsctl.Obs()
        if self.source == 'display':
            obsctl.set_source(self.o, 'monitor_capture', {'monitor_id': 'DUMMY'})
            mon = [i for i in self.o.call('GetInputPropertiesListPropertyItems', {'inputName': 'Capture', 'propertyName': 'monitor_id'})
                   ['propertyItems'] if i['itemEnabled']][0]['itemValue']
            self.o.call('SetInputSettings', {'inputName': 'Capture', 'inputSettings': {'monitor_id': mon, 'method': 0, 'capture_cursor': True}})
        else:
            # The game is a borderless window, which "any full-screen application" does not take.
            obsctl.set_source(self.o, 'game_capture', {'capture_mode': 'window', 'window': 'rbuf game:rbuf-game:game.exe',
                                                       'priority': 2, 'capture_cursor': True})
        time.sleep(2)

    def start(self, tag):
        self.o.call('StartRecord')

    def stop(self):
        r = self.o.call('StopRecord')
        time.sleep(1)
        self.stats = self.o.call('GetStats')
        return r.get('outputPath')

    def cleanup(self):
        try:
            self.o.close()
        except Exception:
            pass
        subprocess.run(['taskkill', '/F', '/IM', 'obs64.exe'], capture_output=True)
        time.sleep(2)


class ShadowPlay:
    family = 'shadowplay'

    def prepare(self):
        pass

    def start(self, tag):
        self.t0 = time.time()
        spctl.toggle_record()

    def stop(self):
        spctl.toggle_record()
        time.sleep(4)
        return spctl.newest(self.t0)

    def cleanup(self):
        pass


class Nothing:
    family = 'none'

    def prepare(self):
        pass

    def start(self, tag):
        pass

    def stop(self):
        return None

    def cleanup(self):
        pass


CONDITIONS = {
    'none': (False, lambda: Nothing()),
    'rbuf-nvfbc': (False, lambda: Rbuf('nvfbc')),
    # NvFBC through rbuf's general path (ARGB grab, compute shader, Media Foundation encoder).
    'rbuf-general': (False, lambda: Rbuf('nvfbc', env={'RBUF_GENERAL_PATH': '1'})),
    'rbuf-wgc': (False, lambda: Rbuf('wgc')),
    'rbuf-process': (False, lambda: Rbuf('nvfbc', ['-w', 'process:game.exe'])),
    'obs-display': (False, lambda: Obs('display')),
    'obs-game': (False, lambda: Obs('game')),
    'nvapp-idle': (True, lambda: Nothing()),
    'shadowplay': (True, lambda: ShadowPlay()),
}


PM = os.environ.get('PRESENTMON', 'PresentMon-2.5.1-x64.exe')


def present_stats(path):
    import csv
    import collections
    try:
        rows = list(csv.DictReader(open(path)))
    except OSError:
        return {}
    if not rows:
        return {}
    modes = collections.Counter(r['PresentMode'] for r in rows)
    lat = sorted(float(r['MsUntilDisplayed']) for r in rows if r.get('MsUntilDisplayed') not in (None, '', 'NA'))
    return {'mode': modes.most_common(1)[0][0], 'mode_share': modes.most_common(1)[0][1] / len(rows),
            'latency_ms': lat[len(lat) // 2] if lat else None, 'latency_p95_ms': lat[int(len(lat) * 0.95)] if lat else None}


def run_one(name, load, smi, rnd):
    _, make = CONDITIONS[name]
    rec = make()
    rec.prepare()
    tag = f'{name}_{load.replace(":", "")}_{rnd}'
    game_args = [GAME, str(SECONDS), f'skip:{SKIP}'] + [a for a in load.split('+') if a != 'light']
    g = subprocess.Popen(game_args, stdout=subprocess.PIPE, text=True)
    t_start = time.time()
    time.sleep(2)
    rec.start(tag)
    time.sleep(max(0, SKIP - (time.time() - t_start)))
    names = RECORDER_PROCS[rec.family]
    rp, dwm, gp = procs_named(names), procs_named({'dwm.exe'}), [psutil.Process(g.pid)]
    if TRACE:
        subprocess.run([XPERF, '-on', 'PROC_THREAD+LOADER+PROFILE+DPC+INTERRUPT', '-stackwalk', 'Profile',
                        '-BufferSize', '1024', '-MinBuffers', '256', '-MaxBuffers', '512'], capture_output=True)
    pm_csv = os.path.join(OUT, tag + '.pm.csv')
    pm = subprocess.Popen([PM, '--process_name', 'game.exe', '--output_file', pm_csv, '--timed', str(SECONDS - SKIP - 4),
                           '--terminate_after_timed', '--no_console_stats', '--stop_existing_session'],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    c0, d0, g0, s0, t0 = cpu_of(rp), cpu_of(dwm), cpu_of(gp), psutil.cpu_times(), time.time()
    time.sleep(SECONDS - SKIP - 1.5)
    if TRACE:
        subprocess.run([XPERF, '-d', os.path.join(OUT, tag + '.etl')], capture_output=True)
    rp2 = procs_named(names)
    c1, d1, g1, s1, t1 = cpu_of(rp + [p for p in rp2 if p.pid not in c0]), cpu_of(dwm), cpu_of(gp), psutil.cpu_times(), time.time()
    mem = mem_of(rp2)
    out = g.communicate()[0].strip()
    pm.wait()
    pmr = present_stats(pm_csv)
    path = rec.stop()
    rec.cleanup()
    wall = t1 - t0
    pct = lambda a, b: 100 * sum(b.get(k, 0) - a.get(k, 0) for k in b) / wall
    busy = lambda c: c.user + c.system + getattr(c, 'interrupt', 0) + getattr(c, 'dpc', 0)
    gpu, enc = smi.window(t0, t1)
    fps, low = (float(x.split()[0]) for x in out.replace('1% low', '').split(','))
    r = {'cond': name, 'load': load, 'round': rnd, 'fps': fps, 'low': low,
         'rec_cpu': pct(c0, c1), 'dwm_cpu': pct(d0, d1), 'game_cpu': pct(g0, g1),
         'sys_cpu': 100 * (busy(s1) - busy(s0)) / wall, 'rec_mem_mb': mem / 2**20,
         'gpu': gpu, 'nvenc': enc, **pmr, **{f'out_{k}': v for k, v in probe(path).items()}}
    print(json.dumps(r), flush=True)
    return r


def main():
    load = sys.argv[1]
    rounds = int(sys.argv[2])
    names = sys.argv[3:] or list(CONDITIONS)
    smi = Smi()
    results = []
    blocks = [[n for n in names if not CONDITIONS[n][0]], [n for n in names if CONDITIONS[n][0]]]
    sp_fps = spctl.get_fps()
    was_running = spctl.service_running()
    if blocks[1]:
        spctl.set_fps(FPS)
    try:
        run_all(load, rounds, blocks, smi, results)
    finally:
        if blocks[1]:
            # Put the owner's setting back and restart the service so it reads it.
            spctl.set_fps(sp_fps)
            spctl.restart_service()
        elif was_running:
            # The other block stops the NVIDIA App's service; leave it as it was found.
            spctl.restart_service()
    smi.stop()
    with open(os.path.join(OUT, f'bench_{load.replace(":", "").replace("+", "_")}_{FPS}fps_{int(time.time())}.json'), 'w') as f:
        json.dump(results, f, indent=1)


def run_all(load, rounds, blocks, smi, results):
    for rnd in range(rounds):
        for need_service, block in zip((False, True), blocks):
            if not block:
                continue
            if need_service:
                spctl.restart_service()
            else:
                spctl.stop_service()
                time.sleep(3)
            k = rnd % len(block)
            for name in block[k:] + block[:k]:
                results.append(run_one(name, load, smi, rnd))
                time.sleep(2)


if __name__ == '__main__':
    main()
