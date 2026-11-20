# The SSH bastion of the live tests (scripts/servers.sh): an sshd that
# forwards TCP, one user, a key pair made on first start and a password.
# Test use only; the credentials are in the repository on purpose.
FROM alpine:3.22
RUN apk add --no-cache openssh-server \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh tunnel \
 && echo 'tunnel:keyfold-test' | chpasswd \
 && mkdir -p /keys /home/tunnel/.ssh
COPY entrypoint.sh /entrypoint.sh
ENTRYPOINT ["/bin/sh", "/entrypoint.sh"]
