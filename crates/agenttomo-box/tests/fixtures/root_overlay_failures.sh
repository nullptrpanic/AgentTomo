mkdir -p /records
before=$(sha256sum /etc/ssl/certs/ca-certificates.crt)
if /agenttomo-box run --no-proxy --overlay-root --capture-http --overlay-exempt /records \
    --overlay-exempt /etc/ssl/certs --output-dir /records/conflict \
    -c 'touch /records/ran'; then
    printf 'CA injection must reject a passthrough trust store\n' >&2
    exit 1
fi
test ! -e /records/ran
test "$before" = "$(sha256sum /etc/ssl/certs/ca-certificates.crt)"
for path in relative / /missing-agenttomo-exemption /tmp/../etc; do
    if /agenttomo-box run --no-proxy --overlay-root --overlay-exempt /records \
        --overlay-exempt "$path" --output-dir "/records/invalid-$(date +%s%N)" \
        -c 'touch /records/ran'; then
        printf 'Invalid exemption was accepted: %s\n' "$path" >&2
        exit 1
    fi
    test ! -e /records/ran
done
# Startup must not copy loose boundary files or /usr/sbin into the private layer.
python3 -c 'from pathlib import Path; Path("/agenttomo-large-boundary-file").write_bytes(b"x" * 2097152)'
/agenttomo-box run --no-proxy --overlay-root --overlay-root-size-mib 1 \
    --overlay-exempt /records --output-dir /records/full \
    -c 'test -r /agenttomo-large-boundary-file; touch /records/ran'
test -e /records/ran
rm /records/ran
cp /etc/ssl/certs/ca-certificates.crt /records/bundle.pem
rm /etc/ssl/certs/ca-certificates.crt
ln -s /records/bundle.pem /etc/ssl/certs/ca-certificates.crt
before=$(sha256sum /records/bundle.pem)
if /agenttomo-box run --no-proxy --overlay-root --capture-http --overlay-exempt /records \
    --output-dir /records/symlink -c 'touch /records/ran'; then
    printf 'A trust-store symlink must not allow CA injection into an exemption\n' >&2
    exit 1
fi
test ! -e /records/ran
test "$before" = "$(sha256sum /records/bundle.pem)"
cp /records/bundle.pem /etc/agenttomo-bundle.pem
ln -sfn /etc/agenttomo-bundle.pem /etc/ssl/certs/ca-certificates.crt
before=$(sha256sum /etc/agenttomo-bundle.pem)
if /agenttomo-box run --no-proxy --overlay-root --capture-http --overlay-exempt /records \
    --output-dir /records/uncovered-symlink -c 'touch /records/ran'; then
    printf 'A trust-store symlink must not inject CA into an uncovered /etc file\n' >&2
    exit 1
fi
test ! -e /records/ran
test "$before" = "$(sha256sum /etc/agenttomo-bundle.pem)"
printf 'ROOT_OVERLAY_FAILURES_OK\n'
