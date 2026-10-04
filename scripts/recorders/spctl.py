"""ShadowPlay (NVIDIA App) control for the benchmark: settings in the registry, the record hotkey
through SendInput, the newest recording from its folder."""
import ctypes
import glob
import os
import struct
import subprocess
import time
import winreg

KEY = r'Software\NVIDIA Corporation\Global\ShadowPlay\NVSPCAPS'
VIDEOS = os.path.expandvars(r'%USERPROFILE%\Videos\NVIDIA')


def get(name):
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, KEY) as k:
        return winreg.QueryValueEx(k, name)[0]


def set_bin(name, data):
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, KEY, 0, winreg.KEY_SET_VALUE) as k:
        winreg.SetValueEx(k, name, 0, winreg.REG_BINARY, data)


def set_u32(name, v):
    set_bin(name, struct.pack('<I', v))


def set_f32(name, v):
    set_bin(name, struct.pack('<f', v))


def get_fps():
    """The recording frame rate (a float in REG_BINARY); the service reads it when it starts."""
    return struct.unpack('<f', get('RecordingFPS'))[0]


def set_fps(fps):
    set_f32('RecordingFPS', float(fps))


def restart_service():
    subprocess.run(['powershell', '-NoProfile', '-Command', 'Restart-Service NvContainerLocalSystem -Force'], check=True)
    # The overlay and its capture helper come up some seconds later.
    end = time.time() + 60
    while time.time() < end:
        out = subprocess.run(['tasklist'], capture_output=True, text=True).stdout
        if 'nvsphelper64.exe' in out and 'NVIDIA Overlay.exe' in out:
            time.sleep(5)
            return True
        time.sleep(1)
    return False


def service_running():
    r = subprocess.run(['powershell', '-NoProfile', '-Command', '(Get-Service NvContainerLocalSystem).Status'], capture_output=True, text=True)
    return r.stdout.strip() == 'Running'


def stop_service():
    subprocess.run(['powershell', '-NoProfile', '-Command', 'Stop-Service NvContainerLocalSystem -Force'], check=True)


VK_MENU, VK_F9, KEYUP = 0x12, 0x78, 0x2


def hotkey(*vks):
    u = ctypes.windll.user32
    for vk in vks:
        u.keybd_event(vk, u.MapVirtualKeyW(vk, 0), 0, 0)
        time.sleep(0.03)
    for vk in reversed(vks):
        u.keybd_event(vk, u.MapVirtualKeyW(vk, 0), KEYUP, 0)
        time.sleep(0.03)


def toggle_record():
    hotkey(VK_MENU, VK_F9)


def newest(since):
    files = [f for f in glob.glob(os.path.join(VIDEOS, '**', '*.mp4'), recursive=True) if os.path.getmtime(f) >= since]
    return max(files, key=os.path.getmtime) if files else None
