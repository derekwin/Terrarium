/* Terrarium adversarial-escape probe.
 *
 * One static binary, several modes — each attempts a real escape /
 * privilege primitive inside a confined sandbox and reports the exact
 * outcome (errno + short message) so a test can assert the enforced
 * behavior without guessing from shell stderr text.
 *
 *   escape_probe fdscan                 enumerate open fds 0..127
 *   escape_probe net tcp IP PORT        connect(2) TCP
 *   escape_probe net udp IP PORT        sendto(2) UDP
 *   escape_probe net sendmsg IP PORT    sendmsg(2) with destination
 *   escape_probe net unix PATH          connect(2) AF_UNIX
 *   escape_probe net raw IP             socket(AF_INET, SOCK_RAW)
 *   escape_probe net ping IP            SOCK_DGRAM ICMP ping socket
 *   escape_probe net bind PORT          bind(0.0.0.0:PORT) + listen
 *   escape_probe net vsock CID PORT     connect(2) AF_VSOCK
 *   escape_probe fds N                  open N files, report max fds
 *   escape_probe mem MB                 malloc + touch, report success
 *   escape_probe fork N                 fork until failure, report count
 *
 * Build: gcc -static -O2 -o escape_probe escape_probe.c
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>
#include <linux/vm_sockets.h>

static void fdscan(void) {
    printf("pid=%d\n", getpid());
    for (int fd = 0; fd < 128; fd++) {
        int fl = fcntl(fd, F_GETFD);
        if (fl == -1) continue;
        struct stat st;
        if (fstat(fd, &st) == 0) {
            printf("FD %d flags=0x%x mode=%o\n", fd, fl, st.st_mode & 0170000);
        } else {
            printf("FD %d flags=0x%x (fstat errno=%d)\n", fd, fl, errno);
        }
    }
}

static void report(const char *op, int rc) {
    printf("%s rc=%d errno=%d (%s)\n", op, rc, errno, strerror(errno));
}

static int net_tcp(const char *ip, int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { report("socket", fd); return 1; }
    struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET; sa.sin_port = htons(port);
    inet_pton(AF_INET, ip, &sa.sin_addr);
    int r = connect(fd, (struct sockaddr *)&sa, sizeof(sa));
    report("connect", r);
    if (r == 0) close(fd);
    return r == 0 ? 0 : 1;
}

static int net_udp(const char *ip, int port) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) { report("socket", fd); return 1; }
    struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET; sa.sin_port = htons(port);
    inet_pton(AF_INET, ip, &sa.sin_addr);
    char buf[8] = "ping";
    int r = sendto(fd, buf, 4, 0, (struct sockaddr *)&sa, sizeof(sa));
    report("sendto", r);
    return r >= 0 ? 0 : 1;
}

static int net_sendmsg(const char *ip, int port) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) { report("socket", fd); return 1; }
    struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET; sa.sin_port = htons(port);
    inet_pton(AF_INET, ip, &sa.sin_addr);
    struct iovec iov; char buf[8] = "ping";
    iov.iov_base = buf; iov.iov_len = 4;
    struct msghdr mh; memset(&mh, 0, sizeof(mh));
    mh.msg_name = &sa; mh.msg_namelen = sizeof(sa);
    mh.msg_iov = &iov; mh.msg_iovlen = 1;
    int r = sendmsg(fd, &mh, 0);
    report("sendmsg", r);
    return r >= 0 ? 0 : 1;
}

static int net_unix(const char *path) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) { report("socket", fd); return 1; }
    struct sockaddr_un su; memset(&su, 0, sizeof(su));
    su.sun_family = AF_UNIX;
    strncpy(su.sun_path, path, sizeof(su.sun_path) - 1);
    int r = connect(fd, (struct sockaddr *)&su, sizeof(su));
    report("unix-connect", r);
    return r == 0 ? 0 : 1;
}

static int net_raw(const char *ip) {
    int fd = socket(AF_INET, SOCK_RAW, IPPROTO_RAW);
    report("raw-socket", fd);
    if (fd >= 0) {
        struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
        sa.sin_family = AF_INET;
        inet_pton(AF_INET, ip, &sa.sin_addr);
        char buf[64] = {0};
        int r = sendto(fd, buf, sizeof(buf), 0, (struct sockaddr *)&sa, sizeof(sa));
        report("raw-sendto", r);
        return r >= 0 ? 0 : 1;
    }
    return 1;
}

static int net_ping(const char *ip) {
    int fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    report("ping-socket", fd);
    if (fd >= 0) {
        struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
        sa.sin_family = AF_INET;
        inet_pton(AF_INET, ip, &sa.sin_addr);
        char buf[64] = {0};
        int r = sendto(fd, buf, sizeof(buf), 0, (struct sockaddr *)&sa, sizeof(sa));
        report("ping-sendto", r);
        return r >= 0 ? 0 : 1;
    }
    return 1;
}

static int net_bind(int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { report("socket", fd); return 1; }
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in sa; memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET; sa.sin_addr.s_addr = htonl(INADDR_ANY);
    sa.sin_port = htons(port);
    int r = bind(fd, (struct sockaddr *)&sa, sizeof(sa));
    report("bind", r);
    if (r == 0) { r = listen(fd, 4); report("listen", r); }
    return r == 0 ? 0 : 1;
}

static int net_vsock(unsigned int cid, int port) {
    int fd = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (fd < 0) { report("vsock-socket", fd); return 1; }
    struct sockaddr_vm sv; memset(&sv, 0, sizeof(sv));
    sv.svm_family = AF_VSOCK;
    sv.svm_cid = cid;
    sv.svm_port = port;
    int r = connect(fd, (struct sockaddr *)&sv, sizeof(sv));
    report("vsock-connect", r);
    return r == 0 ? 0 : 1;
}

static void fds_limit(int n) {
    int opened = 0;
    for (int i = 0; i < n; i++) {
        char path[64];
        snprintf(path, sizeof(path), "/tmp/esc-fd-%d", i);
        int fd = open(path, O_CREAT | O_RDWR, 0600);
        if (fd < 0) {
            printf("open %d failed at %d errno=%d (%s)\n", n, opened, errno, strerror(errno));
            return;
        }
        opened++;
    }
    printf("opened %d/%d fds\n", opened, n);
}

static int mem_limit(int mb) {
    size_t bytes = (size_t)mb * 1024 * 1024;
    char *p = malloc(bytes);
    if (!p) { printf("malloc %dMB failed errno=%d (%s)\n", mb, errno, strerror(errno)); return 1; }
    for (size_t i = 0; i < bytes; i += 4096) p[i] = 1;
    p[bytes - 1] = 1;
    printf("touched %dMB ok\n", mb);
    return 0;
}

static void fork_limit(int n) {
    int forked = 0;
    for (int i = 0; i < n; i++) {
        pid_t p = fork();
        if (p == 0) { _exit(0); }
        if (p < 0) {
            printf("fork %d failed at %d errno=%d (%s)\n", n, forked, errno, strerror(errno));
            return;
        }
        forked++;
    }
    printf("forked %d/%d\n", forked, n);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        printf("usage: escape_probe MODE [args...]\n");
        return 2;
    }
    const char *m = argv[1];
    if (!strcmp(m, "fdscan")) { fdscan(); return 0; }
    if (!strcmp(m, "net") && argc >= 5) {
        const char *proto = argv[2];
        const char *ip = argv[3];
        int port = atoi(argv[4]);
        if (!strcmp(proto, "tcp")) return net_tcp(ip, port);
        else if (!strcmp(proto, "udp")) return net_udp(ip, port);
        else if (!strcmp(proto, "sendmsg")) return net_sendmsg(ip, port);
        else { printf("unknown net proto %s\n", proto); return 2; }
    }
    if (!strcmp(m, "net") && argc == 4 && !strcmp(argv[2], "raw")) {
        return net_raw(argv[3]);
    }
    if (!strcmp(m, "net") && argc == 4 && !strcmp(argv[2], "ping")) {
        return net_ping(argv[3]);
    }
    if (!strcmp(m, "net") && argc == 4 && !strcmp(argv[2], "bind")) {
        return net_bind(atoi(argv[3]));
    }
    if (!strcmp(m, "net") && argc >= 4 && !strcmp(argv[2], "unix")) {
        return net_unix(argv[3]);
    }
    if (!strcmp(m, "net") && argc == 4 && !strcmp(argv[2], "vsock")) {
        return net_vsock((unsigned int)atoi(argv[3]), 1024);
    }
    if (!strcmp(m, "fds") && argc == 3) { fds_limit(atoi(argv[2])); return 0; }
    if (!strcmp(m, "mem") && argc == 3) { return mem_limit(atoi(argv[2])); }
    if (!strcmp(m, "fork") && argc == 3) { fork_limit(atoi(argv[2])); return 0; }
    printf("bad arguments\n");
    return 2;
}
