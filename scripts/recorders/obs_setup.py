"""Writes a portable OBS configuration for the benchmark: profile "Bench" (advanced output, NVENC
H.264 CBR, 1920x1080 at 60 fps, one desktop audio track, MP4), an empty scene collection "Bench"
(the harness adds sources over obs-websocket), and obs-websocket on port 4455 without a password.

    python obs_setup.py [bitrate_kbps] [preset] [cbr|vbr]
"""
import json
import os
import sys

kbps = int(sys.argv[1]) if len(sys.argv) > 1 else 20000
preset = sys.argv[2] if len(sys.argv) > 2 else 'p5'
rc = (sys.argv[3] if len(sys.argv) > 3 else 'cbr').upper()
# Portable mode: the configuration lives next to OBS, so a normal OBS setup is left alone.
root = os.path.join(os.environ.get('OBS_STUDIO', r'C:\Program Files\obs-studio'), 'config', 'obs-studio')
# OBS reads backslash escapes in its ini files, so the path uses forward slashes.
out_dir = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..', 'target', 'recorders', 'obs')).replace('\\', '/')
os.makedirs(out_dir, exist_ok=True)


def write(rel, text):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, 'w', encoding='utf-8') as f:
        f.write(text)


general = '''[General]
FirstRun=true
LastVersion=553713666
EnableAutoUpdates=false
Pre19Defaults=false
Pre21Defaults=false
Pre23Defaults=false
Pre24.1Defaults=false
MaxLogs=10
InfoIncrement=-1
ProcessPriority=Normal
EnableAutoUpdates=false
ConfirmOnExit=false
WarnBeforeStartingRecord=false
WarnBeforeStoppingRecord=false

[Basic]
Profile=Bench
ProfileDir=Bench
SceneCollection=Bench
SceneCollectionFile=Bench
ConfigOnNewProfile=true

[BasicWindow]
SysTrayEnabled=true
SysTrayWhenStarted=false
SysTrayMinimizeToTray=true
'''
write('global.ini', general)
write('user.ini', general)

write(r'basic\profiles\Bench\basic.ini', f'''[General]
Name=Bench

[Output]
Mode=Advanced

[AdvOut]
RecType=Standard
RecFilePath={out_dir}
RecFormat2=mp4
RecEncoder=obs_nvenc_h264_tex
RecTracks=1
RecUseRescale=false
TrackIndex=1
RecAudioEncoder=ffmpeg_aac
Track1Bitrate=192

[SimpleOutput]
FilePath={out_dir}
RecFormat2=mp4

[Video]
BaseCX=1920
BaseCY=1080
OutputCX=1920
OutputCY=1080
FPSType=1
FPSInt=60
FPSCommon=60
ScaleType=bicubic
ColorFormat=NV12
ColorSpace=709
ColorRange=Partial

[Audio]
SampleRate=48000
ChannelSetup=Stereo
''')

write(r'basic\profiles\Bench\recordEncoder.json', json.dumps({
    'rate_control': rc,
    'bitrate': kbps,
    'max_bitrate': kbps * 2,
    'preset2': preset,
    'tune': 'hq',
    'multipass': 'disabled',
    'profile': 'high',
    'keyint_sec': 1,
    'bf': 0,
}))

write(r'basic\scenes\Bench.json', json.dumps({
    'name': 'Bench',
    'current_scene': 'Scene',
    'current_program_scene': 'Scene',
    'scene_order': [{'name': 'Scene'}],
    'sources': [{
        'id': 'scene', 'versioned_id': 'scene', 'name': 'Scene', 'uuid': '6b2b3a2e-0d47-4a39-9a4f-1c1c1c1c0001',
        'settings': {'id_counter': 0, 'items': []}, 'enabled': True, 'flags': 0, 'volume': 1.0, 'mixers': 0,
    }],
    'DesktopAudioDevice1': {
        'id': 'wasapi_output_capture', 'versioned_id': 'wasapi_output_capture', 'name': 'Desktop Audio',
        'uuid': '6b2b3a2e-0d47-4a39-9a4f-1c1c1c1c0002', 'settings': {'device_id': 'default'}, 'enabled': True,
        'mixers': 1, 'volume': 1.0, 'flags': 0,
    },
    'transitions': [],
    'current_transition': 'Fade',
    'transition_duration': 300,
}))

write(r'plugin_config\obs-websocket\config.json', json.dumps({
    'server_enabled': True, 'server_port': 4455, 'auth_required': False, 'server_password': '',
    'first_load': False, 'alerts_enabled': False,
}))
print('written', root, kbps, preset)
