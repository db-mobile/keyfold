#!/bin/sh
# usage: entrypoint.sh <port>
set -eu
PORT="${1:-22}"
if [ ! -f /keys/id_ed25519 ]; then
  ssh-keygen -q -t ed25519 -N "" -C keyfold-test -f /keys/id_ed25519
  # a second key that needs its passphrase
  ssh-keygen -q -t ed25519 -N "keyfold-pass" -C keyfold-test-pass -f /keys/id_ed25519_pass
fi
cat /keys/id_ed25519.pub /keys/id_ed25519_pass.pub > /home/tunnel/.ssh/authorized_keys
chown -R tunnel:tunnel /home/tunnel/.ssh
chmod 700 /home/tunnel/.ssh
chmod 600 /home/tunnel/.ssh/authorized_keys
exec /usr/sbin/sshd -D -e -p "$PORT" \
  -o ListenAddress=127.0.0.1 \
  -o AllowTcpForwarding=yes \
  -o PasswordAuthentication=yes \
  -o PermitRootLogin=no \
  -o MaxSessions=64 \
  -o MaxStartups=64
