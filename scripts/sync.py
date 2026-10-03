"""Measures audio/video sync in a recording of `examples/sync.rs`: the time of each white flash in
the video against the click in the first audio track. Prints each pair and the mean offset
(positive: the sound comes after the picture). Needs ffmpeg on PATH.

    python scripts/sync.py recording.mp4
"""
import struct
import subprocess
import sys

path = sys.argv[1]
probe = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries", "stream=width,height,avg_frame_rate",
                        "-of", "csv=p=0", path], capture_output=True, text=True).stdout.strip().split(",")
w, h = int(probe[0]), int(probe[1])
num, den = map(int, probe[2].split("/"))
fps = num / den
# Mean brightness of the middle of each frame, and every frame's presentation time.
raw = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-vf", f"crop={w//2}:{h//2}:{w//4}:{h//4},scale=8:8,format=gray",
                      "-f", "rawvideo", "-"], capture_output=True).stdout
times = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v", "-show_entries", "frame=pts_time", "-of", "csv=p=0", path],
                       capture_output=True, text=True).stdout.split()
lum = [sum(raw[i * 64:(i + 1) * 64]) / 64 for i in range(len(raw) // 64)]
flashes = [float(times[i]) for i in range(1, len(lum)) if lum[i] > 128 and lum[i - 1] <= 128]

pcm = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "s16le", "-"],
                     capture_output=True).stdout
start = float(subprocess.run(["ffprobe", "-v", "error", "-select_streams", "a:0", "-show_entries", "stream=start_time", "-of", "csv=p=0", path],
                             capture_output=True, text=True).stdout.strip() or 0)
s = struct.unpack("<%dh" % (len(pcm) // 2), pcm)
clicks = []
quiet = 0
for i, x in enumerate(s):
    if abs(x) > 3000 and quiet > 4800:
        clicks.append(start + i / 48000)
    quiet = 0 if abs(x) > 3000 else quiet + 1

offs = []
for f in flashes:
    near = min(clicks, key=lambda c: abs(c - f), default=None)
    if near is not None and abs(near - f) < 0.4:
        offs.append((near - f) * 1000)
        print(f"flash {f:7.3f} s  click {near:7.3f} s  offset {(near - f) * 1000:+6.1f} ms")
if offs:
    offs.sort()
    print(f"{len(offs)} pairs: mean {sum(offs) / len(offs):+.1f} ms, min {offs[0]:+.1f}, max {offs[-1]:+.1f} (one frame is {1000 / fps:.1f} ms)")
else:
    print("no flash/click pairs found")
