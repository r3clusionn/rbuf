"""Minimal obs-websocket v5 client and OBS launcher for the benchmark."""
import json
import os
import subprocess
import time
import uuid

import websocket

OBS_DIR = os.path.join(os.environ.get('OBS_STUDIO', r'C:\Program Files\obs-studio'), 'bin', '64bit')


class Obs:
    def __init__(self, url='ws://127.0.0.1:4455', timeout=30):
        end = time.time() + timeout
        while True:
            try:
                self.ws = websocket.create_connection(url, timeout=10)
                break
            except OSError:
                if time.time() > end:
                    raise
                time.sleep(0.5)
        hello = json.loads(self.ws.recv())
        assert hello['op'] == 0, hello
        self.ws.send(json.dumps({'op': 1, 'd': {'rpcVersion': 1, 'eventSubscriptions': 0}}))
        ident = json.loads(self.ws.recv())
        assert ident['op'] == 2, ident

    def call(self, kind, data=None, check=True):
        rid = str(uuid.uuid4())
        self.ws.send(json.dumps({'op': 6, 'd': {'requestType': kind, 'requestId': rid, 'requestData': data or {}}}))
        while True:
            m = json.loads(self.ws.recv())
            if m['op'] == 7 and m['d']['requestId'] == rid:
                st = m['d']['requestStatus']
                if check and not st['result']:
                    raise RuntimeError(f'{kind}: {st}')
                return m['d'].get('responseData', {})

    def close(self):
        self.ws.close()


def launch():
    # A forced kill leaves run markers behind, and OBS then opens a "crash detected" dialog.
    import glob
    import os
    for f in glob.glob(os.path.join(OBS_DIR, '..', '..', 'config', 'obs-studio', '.sentinel', 'run_*')):
        os.remove(f)
    return subprocess.Popen(
        [OBS_DIR + r'\obs64.exe', '--portable', '--minimize-to-tray', '--disable-shutdown-check', '--disable-updater',
         '--profile', 'Bench', '--collection', 'Bench', '--scene', 'Scene'],
        cwd=OBS_DIR)


def set_source(obs, kind, settings):
    """Leaves exactly one capture source, `kind` with `settings`, in the scene."""
    for item in obs.call('GetSceneItemList', {'sceneName': 'Scene'})['sceneItems']:
        obs.call('RemoveInput', {'inputName': item['sourceName']})
    time.sleep(0.5)
    obs.call('CreateInput', {'sceneName': 'Scene', 'inputName': 'Capture', 'inputKind': kind, 'inputSettings': settings})


if __name__ == '__main__':
    p = launch()
    o = Obs()
    print(o.call('GetVersion')['obsVersion'])
    print(o.call('GetRecordDirectory'))
    print(o.call('GetVideoSettings'))
    print([i['inputKind'] for i in o.call('GetInputList')['inputs']])
    o.call('CreateInput', {'sceneName': 'Scene', 'inputName': 'Probe', 'inputKind': 'monitor_capture', 'inputSettings': {}})
    print(o.call('GetInputPropertiesListPropertyItems', {'inputName': 'Probe', 'propertyName': 'monitor_id'}))
    print(o.call('GetInputPropertiesListPropertyItems', {'inputName': 'Probe', 'propertyName': 'method'}))
    print(o.call('GetInputSettings', {'inputName': 'Probe'}))
    print(o.call('GetProfileParameter', {'parameterCategory': 'AdvOut', 'parameterName': 'RecEncoder'}))
    o.close()
