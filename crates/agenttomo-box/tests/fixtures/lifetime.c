// Real Linux lifecycle cases that cannot be expressed reliably by a shell.
#include <fcntl.h>
#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void pid_file(const char *name) {
    FILE *file = fopen(name, "w");
    if (!file || fprintf(file, "%d\n", getpid()) < 0 || fclose(file)) _exit(90);
}

static void gate(void) {
    alarm(15); // Bound cleanup even if the test fails before releasing us.
    while (access("release", F_OK)) usleep(1000);
    alarm(0);
}

static void *thread_main(void *arg) {
    gate();
    if (arg) execl("/bin/echo", "thread-exec", "thread-output", NULL);
    else puts("thread-output");
    return NULL;
}

int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "deny-ptrace")) {
        struct sock_filter rules[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_ptrace, 0, 1),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        };
        struct sock_fprog filter = { .len = sizeof(rules) / sizeof(rules[0]), .filter = rules };
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)
            || prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &filter)) return 100;
        execl(argv[2], argv[2], "run", "-c", "touch should-not-exist", "--output-dir", "run", NULL);
        return 101;
    }
    if (argc != 2) return 91;
    pid_file("root.pid");
    if (!strcmp(argv[1], "daemon")) {
        pid_t child = fork();
        if (child < 0) return 92;
        if (!child) {
            if (setsid() < 0) _exit(93);
            child = fork();
            if (child < 0) _exit(94);
            if (child) _exit(0);
            close(0); close(1); close(2);
            pid_file("descendant.pid");
            gate();
            execl("/bin/true", "survivor-after-root", NULL);
            _exit(95);
        }
        while (access("descendant.pid", F_OK)) usleep(1000);
        return 27;
    }
    if (!strcmp(argv[1], "thread") || !strcmp(argv[1], "thread-exec")) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, thread_main,
                           !strcmp(argv[1], "thread-exec") ? (void *)1 : NULL)) return 96;
        pthread_exit(NULL);
    }
    if (!strcmp(argv[1], "stop")) {
        puts("ready"); fflush(stdout);
        raise(SIGSTOP);
        puts("resumed");
        return 0;
    }
    if (!strcmp(argv[1], "busy")) {
        pid_t child = fork();
        if (child < 0) return 97;
        if (!child) {
            alarm(15);
            char bytes[4096]; memset(bytes, 'x', sizeof(bytes));
            while (access("release", F_OK)) {
                if (write(1, bytes, sizeof(bytes)) < 0) _exit(98);
            }
            return 0;
        }
        puts("root-output");
        return 31;
    }
    if (!strcmp(argv[1], "wait")) {
        gate();
        return 0;
    }
    return 99;
}
