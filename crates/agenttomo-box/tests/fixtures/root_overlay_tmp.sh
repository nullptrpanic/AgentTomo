mkdir -p /records
/agenttomo-box run --no-proxy --overlay-root --overlay-exempt /records \
    --output-dir /records/whole-tmp -c '
set -eu
python3 -c "import pathlib; assert all(not list(p.iterdir()) for p in pathlib.Path(\"/tmp\").glob(\"agenttomo-root-*\")), \"backing store exposed through private /tmp\""
printf private >/tmp/agenttomo-private-tmp-file
'
test ! -e /tmp/agenttomo-private-tmp-file
/agenttomo-box run --no-proxy --overlay-root --overlay-exempt /tmp --overlay-exempt /records \
    --output-dir /records/run -c '
set -eu
python3 -c "import pathlib; anchors=list(pathlib.Path(\"/tmp\").glob(\"agenttomo-root-*\")); assert all(not list(p.iterdir()) for p in anchors), \"backing store exposed through exemption\""
printf shared >/tmp/agenttomo-shared-file
printf private >/root/agenttomo-private-file
'
test "$(cat /tmp/agenttomo-shared-file)" = shared
test ! -e /root/agenttomo-private-file
ln -s /var/tmp /tmp/agenttomo-tmp-alias
TMPDIR=/tmp/agenttomo-tmp-alias /agenttomo-box run --no-proxy --overlay-root --overlay-exempt /var/tmp --overlay-exempt /records \
    --output-dir /records/alias -c '
python3 -c "import pathlib; assert all(not list(p.iterdir()) for p in pathlib.Path(\"/var/tmp\").glob(\"agenttomo-root-*\")), \"aliased backing store exposed\""
'
printf 'ROOT_OVERLAY_TMP_OK\n'
