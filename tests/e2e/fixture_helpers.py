import json
import sys
import uuid
from support import ROOT


def encode(text):
    return text.encode('latin1') if all(ord(ch) < 256 for ch in text) else text.encode()


def fixture(lab, **config):
    base = lab.directory / uuid.uuid4().hex
    config['base'] = str(base)
    path = base.with_suffix('.json')
    path.write_text(json.dumps(config))
    sid, pid = lab.start([sys.executable, str(ROOT / 'tests/e2e/fixture.py'), str(path)])
    return sid, pid, base
