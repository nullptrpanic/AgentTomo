mkdir -m 1777 /records
setpriv --reuid=12345 --regid=12345 --clear-groups /bin/sh -eu <<'USER'
if /agenttomo-box run --no-proxy --overlay-root --overlay-exempt /records \
    --output-dir /records/run -c '
        set -eu
        touch /records/target-started
        test "$(stat -c %u /usr)" = "$(cat /proc/sys/kernel/overflowuid)"
        test "$(stat -c %g /usr)" = "$(cat /proc/sys/kernel/overflowgid)"
        touch /tmp/private-single-id-file
    ' 2>/records/error; then
    test ! -e /tmp/private-single-id-file
else
    cat /records/error >&2
    grep -q 'cannot preserve owner.*user namespace ID mapping' /records/error
    test ! -e /records/target-started
fi
USER
echo ROOT_OVERLAY_SINGLE_ID_OK
