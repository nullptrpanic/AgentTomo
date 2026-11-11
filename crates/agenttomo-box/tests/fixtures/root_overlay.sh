set -x
mkdir -p /root/overlay-fixture /tmp/overlay-pass /tmp/overlay-dir /records
chmod 0750 /root
printf lower >/etc/agenttomo-overlay-fixture
printf lower >/etc/ssl/agenttomo-overlay-fixture
printf lower >/usr/sbin/agenttomo-overlay-fixture
printf lower >/root/overlay-fixture/modify
printf lower >/root/overlay-fixture/delete
printf lower >/tmp/overlay-dir/original
printf lower >/root/agenttomo-root-loose
printf lower >/agenttomo-root-loose
printf original >/tmp/overlay-pass/value
cd /root/overlay-fixture
/agenttomo-box run --no-proxy --overlay-root --overlay-root-size-mib 64 \
    --overlay-exempt /tmp/overlay-pass --overlay-exempt /records \
    --capture-http --output-dir /records/run -c '
set -eux
test "$PWD" = /root/overlay-fixture
test "$(stat -c %a /root)" = 750
python3 -c "from pathlib import Path; rows=[r.split() for r in Path(\"/proc/self/mountinfo\").read_text().splitlines()]; overlays={r[4] for r in rows if r[r.index(\"-\")+1]==\"overlay\"}; assert \"/root\" in overlays and \"/etc/ssl\" in overlays, overlays; assert all(p==\"/etc/ssl\" or Path(p).parent==Path(\"/\") for p in overlays), overlays; assert not overlays.intersection({\"/etc\", \"/usr\", \"/tmp\", \"/readonly\"}), overlays"
test "$(cat modify)" = lower
printf upper >modify
rm delete
printf new >created
mv created renamed
ln modify hardlink
printf linked >hardlink
test "$(cat modify)" = linked
printf upper >/etc/agenttomo-overlay-fixture
printf upper >/etc/ssl/agenttomo-overlay-fixture
printf upper >/usr/sbin/agenttomo-overlay-fixture
printf upper >/root/agenttomo-root-loose
printf upper >/agenttomo-root-loose
printf private >/readonly/data/created
test "$(cat /readonly/license)" = readonly-original
python3 -c "import os; assert os.statvfs(\"/readonly\").f_flag & os.ST_RDONLY; assert not os.statvfs(\"/readonly/data\").f_flag & os.ST_RDONLY"
printf upper >/tmp/overlay-dir/original
printf shared >/tmp/overlay-pass/value
test -c /dev/null
test -d /proc/self/fd
test -d /sys/class
test -r /etc/resolv.conf
test -r /etc/ssl/certs/agenttomo-task-ca.pem
test ! -w /agenttomo-box
python3 -c "import os; assert os.statvfs(\"/agenttomo-box\").f_flag & os.ST_RDONLY"
test "$(findmnt -n -o FSTYPE -T /root/overlay-fixture/modify)" = overlay
test "$(cat /proc/self/status | sed -n "s/^CapEff:[[:space:]]*//p")" = 00000000000000c0
'
test "$(cat /etc/agenttomo-overlay-fixture)" = upper
test "$(cat /etc/ssl/agenttomo-overlay-fixture)" = lower
test "$(cat /usr/sbin/agenttomo-overlay-fixture)" = upper
test "$(cat /root/overlay-fixture/modify)" = lower
test -f /root/overlay-fixture/delete
test ! -e /root/overlay-fixture/renamed
test "$(cat /root/agenttomo-root-loose)" = lower
test "$(cat /agenttomo-root-loose)" = upper
test "$(cat /readonly/data/created)" = private
test "$(cat /tmp/overlay-dir/original)" = upper
test "$(cat /tmp/overlay-pass/value)" = shared
test ! -e /etc/ssl/certs/agenttomo-task-ca.pem
python3 - <<'PY'
import json, pathlib, time
run = pathlib.Path('/records/run')
for _ in range(200):
    status = json.loads((run / 'collection-status.json').read_text())
    assert status['state'] != 'failed', status
    if status['state'] == 'complete': break
    time.sleep(.025)
else: raise AssertionError('collection did not finish')
events = [json.loads(line) for line in (run / 'events.jsonl').read_text().splitlines()]
assert any(e['kind'] == 'file.open' and e['data'].get('path') == '/etc/agenttomo-overlay-fixture' for e in events)
assert not list(pathlib.Path('/tmp').glob('agenttomo-root-*')), 'task storage was not removed'
PY
printf 'ROOT_OVERLAY_OK\n'
