mkdir -p /records /root/overlay-fixture
printf original >/root/overlay-fixture/value
cat >/root/overlay-fixture/task.py <<'PY'
import errno, os, pathlib, time
if os.fork(): os._exit(0)
for _ in range(200):
    if pathlib.Path('/records/continue').exists(): break
    time.sleep(.025)
else: raise AssertionError('foreground did not return before descendant')
pathlib.Path('/root/overlay-fixture/value').write_text('child-private')
path = pathlib.Path('/root/overlay-fixture/full')
try:
    with path.open('wb', buffering=0) as f:
        for _ in range(128): f.write(b'x' * 1048576)
except OSError as error:
    assert error.errno == errno.ENOSPC, error
else: raise AssertionError('task storage limit was not enforced')
path.unlink()
pathlib.Path('/records/child-result').write_text(pathlib.Path('/root/overlay-fixture/value').read_text())
PY
/agenttomo-box run --no-proxy --overlay-root --overlay-root-size-mib 64 --overlay-exempt /records \
    --output-dir /records/run -c 'exec python3 /root/overlay-fixture/task.py'
test "$(cat /root/overlay-fixture/value)" = original
test ! -e /root/overlay-fixture/full
python3 - <<'PY'
import pathlib
assert list(pathlib.Path('/tmp').glob('agenttomo-root-*')), 'storage was released at root exit'
assert not pathlib.Path('/records/child-result').exists()
pathlib.Path('/records/continue').touch()
PY
python3 - <<'PY'
import json, pathlib, time
for _ in range(400):
    status = json.loads(pathlib.Path('/records/run/collection-status.json').read_text())
    assert status['state'] != 'failed', status
    if status['state'] == 'complete': break
    time.sleep(.025)
else: raise AssertionError('descendant collection did not finish')
assert pathlib.Path('/records/child-result').read_text() == 'child-private'
assert pathlib.Path('/root/overlay-fixture/value').read_text() == 'original'
assert not list(pathlib.Path('/tmp').glob('agenttomo-root-*'))
PY
printf 'ROOT_OVERLAY_LIFETIME_OK\n'
