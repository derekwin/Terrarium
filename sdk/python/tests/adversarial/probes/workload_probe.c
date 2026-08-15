/* Deterministic agent-style workload probe (static, portable).
 *
 * One binary, identical code in every environment (host / VM / sandbox /
 * docker / gVisor) — no shell/interpreter variance. Modes:
 *
 *   workload_probe fileio <dir> <n>   create+read+unlink n small files
 *   workload_probe subproc <dir> <n>  fork+exec /bin/true n times
 *   workload_probe cpu <dir> <n>      n iterations of integer arithmetic
 *   workload_probe mixed <dir> <n>    write files, spawn a build child,
 *                                     read its output, repeat n times
 *
 * Prints "done-<mode>" on success. Build: gcc -static -O2.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static void fileio(const char *dir, int n) {
    char path[512];
    for (int i = 0; i < n; i++) {
        snprintf(path, sizeof(path), "%s/wf%d", dir, i);
        int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0600);
        if (fd < 0) { perror("open"); exit(1); }
        char buf[64];
        int len = snprintf(buf, sizeof(buf), "line-%d payload 0123456789\n", i);
        if (write(fd, buf, len) != len) { perror("write"); exit(1); }
        close(fd);
        fd = open(path, O_RDONLY);
        if (fd < 0) { perror("open-r"); exit(1); }
        char r[64];
        if (read(fd, r, sizeof(r)) < 0) { perror("read"); exit(1); }
        close(fd);
        if (unlink(path) != 0) { perror("unlink"); exit(1); }
    }
}

static void subproc(int n) {
    for (int i = 0; i < n; i++) {
        pid_t p = fork();
        if (p == 0) {
            execl("/bin/true", "true", (char *)NULL);
            _exit(127);
        }
        int st;
        if (p < 0 || waitpid(p, &st, 0) < 0) { perror("wait"); exit(1); }
    }
}

static void cpu(int n) {
    volatile unsigned long acc = 0;
    for (int i = 0; i < n; i++) {
        acc = acc * 31 + (unsigned long)i;
    }
    if (acc == 0xffffffffffffffffUL) exit(1); /* never true; keep the loop */
}

static void mixed(const char *dir, int n) {
    char path[512];
    for (int round = 0; round < n; round++) {
        for (int j = 0; j < 20; j++) {
            snprintf(path, sizeof(path), "%s/src/m%d_%d", dir, round, j);
            int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0600);
            char buf[64];
            int len = snprintf(buf, sizeof(buf), "def f(): return %d\n", round + j);
            if (fd < 0 || write(fd, buf, len) != len) { perror("write"); exit(1); }
            close(fd);
        }
        /* "build": a child counts the sources and writes a summary */
        pid_t p = fork();
        if (p == 0) {
            char cmd[600];
            snprintf(cmd, sizeof(cmd), "ls %s/src | wc -l > %s/build.log", dir, dir);
            execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
            _exit(127);
        }
        int st;
        if (p < 0 || waitpid(p, &st, 0) < 0) { perror("wait"); exit(1); }
        snprintf(path, sizeof(path), "%s/build.log", dir);
        int fd = open(path, O_RDONLY);
        if (fd < 0) { perror("open-log"); exit(1); }
        char r[16];
        if (read(fd, r, sizeof(r)) < 0) { perror("read-log"); exit(1); }
        close(fd);
        unlink(path);
    }
}

int main(int argc, char **argv) {
    if (argc < 3) { printf("usage: workload_probe MODE DIR N\n"); return 2; }
    const char *mode = argv[1];
    const char *dir = argv[2];
    int n = atoi(argv[3]);
    if (!strcmp(mode, "fileio") || !strcmp(mode, "mixed")) {
        if (mkdir(dir, 0700) != 0 && errno != EEXIST) {
            perror("mkdir");
            return 1;
        }
        if (!strcmp(mode, "mixed")) {
            char src[512];
            snprintf(src, sizeof(src), "%s/src", dir);
            if (mkdir(src, 0700) != 0 && errno != EEXIST) {
                perror("mkdir-src");
                return 1;
            }
        }
    }
    if (!strcmp(mode, "fileio")) fileio(dir, n);
    else if (!strcmp(mode, "subproc")) subproc(n);
    else if (!strcmp(mode, "cpu")) cpu(n);
    else if (!strcmp(mode, "mixed")) mixed(dir, n);
    else { printf("unknown mode %s\n", mode); return 2; }
    printf("done-%s\n", mode);
    return 0;
}
