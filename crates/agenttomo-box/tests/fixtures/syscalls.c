#define _GNU_SOURCE
#include <errno.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/openat2.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <sys/syscall.h>
#include <unistd.h>

static void check(int result, const char *operation) {
    if (result < 0) { perror(operation); exit(91); }
}

static int files(void) {
    check(mkdir("files", 0700), "mkdir");
    int directory = open("files", O_RDONLY | O_DIRECTORY);
    check(directory, "open directory");
    int fd = openat(directory, "relative.txt", O_CREAT | O_RDWR, 0600);
    check(fd, "openat");
    check(write(fd, "unchanged", 9), "write");
    close(fd);
    struct open_how how = {.flags = O_RDONLY, .resolve = RESOLVE_BENEATH};
    fd = syscall(SYS_openat2, directory, "relative.txt", &how, sizeof(how));
    check(fd, "openat2");
    char contents[9];
    check(read(fd, contents, sizeof(contents)), "read");
    if (memcmp(contents, "unchanged", 9)) return 92;
    close(fd);
    fd = creat("legacy-created.txt", 0600);
    check(fd, "creat");
    close(fd);
#ifdef SYS_open
    fd = syscall(SYS_open, "legacy-created.txt", O_RDONLY, 0);
    check(fd, "legacy open");
    close(fd);
#endif
    errno = 0;
    if (openat(directory, "missing.txt", O_RDONLY) != -1 || errno != ENOENT) return 93;
    errno = 0;
    if (syscall(SYS_openat, directory, (void *)1, O_RDONLY, 0) != -1 || errno != EFAULT) return 94;
    const char raw_path[] = {'r','a','w','-', (char)0xff, 0};
    fd = openat(directory, raw_path, O_CREAT | O_WRONLY, 0600);
    check(fd, "raw path");
    close(fd);
    close(directory);
    puts("FILES_OK");
    return 0;
}

static const struct sockaddr *destination;
static socklen_t destination_size;
static int family;
static int port;
static int inherited_socket = -1;
static int upload_only;
static int invalid_calls;

static int tcp(int nonblocking, int duplicate, int refused) {
    int fd = inherited_socket >= 0 ? inherited_socket : socket(family, SOCK_STREAM | (nonblocking ? SOCK_NONBLOCK : 0), 0);
    check(fd, "socket");
    if (duplicate) { int other = dup(fd); check(other, "dup"); close(fd); fd = other; }
    unsigned char saved[sizeof(struct sockaddr_in6)];
    memcpy(saved, destination, destination_size);
    int result = connect(fd, destination, destination_size);
    if (result < 0) {
        if (!nonblocking || errno != EINPROGRESS) { perror("connect"); return 80; }
        struct pollfd wait = {.fd = fd, .events = POLLOUT};
        if (poll(&wait, 1, 5000) != 1) return 81;
        int error = 0; socklen_t size = sizeof(error);
        check(getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &size), "SO_ERROR");
        if (error) return 82;
        check(fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) & ~O_NONBLOCK), "blocking io");
    }
    if (memcmp(saved, destination, destination_size)) return 83;
    struct sockaddr_storage peer; socklen_t size = sizeof(peer);
    check(getpeername(fd, (struct sockaddr *)&peer, &size), "getpeername");
    int peer_port = family == AF_INET ? ntohs(((struct sockaddr_in *)&peer)->sin_port) : ntohs(((struct sockaddr_in6 *)&peer)->sin6_port);
    if (peer_port == port) { fputs("connection bypassed proxy\n", stderr); return 84; }
    if (invalid_calls && (connect(fd, destination, destination_size) != -1 || errno != EISCONN)) return 69;
    if (refused) {
        char byte;
        if (recv(fd, &byte, 1, 0) > 0) return 85;
        close(fd); puts("UPSTREAM_CLOSED"); return 0;
    }
    unsigned char bytes[65536];
    for (size_t index = 0; index < sizeof(bytes); index++) bytes[index] = (unsigned char)index;
    size_t offset = 0;
    while (offset < sizeof(bytes)) { ssize_t n = write(fd, bytes + offset, sizeof(bytes) - offset); check(n, "send"); offset += n; }
    if (upload_only) _exit(0);
    check(shutdown(fd, SHUT_WR), "half close");
    unsigned char reply[sizeof(bytes)]; offset = 0;
    while (offset < sizeof(reply)) { ssize_t n = read(fd, reply + offset, sizeof(reply) - offset); check(n, "receive"); if (!n) return 86; offset += n; }
    if (memcmp(reply, bytes, sizeof(bytes))) return 87;
    char extra; if (read(fd, &extra, 1) != 0) return 88;
    close(fd); puts("TCP_OK"); return 0;
}

static void *thread_client(void *unused) {
    (void)unused;
    return (void *)(long)tcp(1, 1, 0);
}

static int network(const char *mode, int requested_port) {
    port = requested_port;
    family = !strcmp(mode, "tcp6") ? AF_INET6 : AF_INET;
    void *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) return 89;
    if (family == AF_INET) {
        struct sockaddr_in value = {.sin_family = AF_INET, .sin_port = htons(port)};
        inet_pton(AF_INET, "127.0.0.1", &value.sin_addr);
        memcpy(page, &value, sizeof(value)); destination_size = sizeof(value);
    } else {
        struct sockaddr_in6 value = {.sin6_family = AF_INET6, .sin6_port = htons(port)};
        inet_pton(AF_INET6, "::1", &value.sin6_addr);
        memcpy(page, &value, sizeof(value)); destination_size = sizeof(value);
    }
    check(mprotect(page, 4096, PROT_READ), "readonly sockaddr");
    destination = page;
    if (!strcmp(mode, "inherited")) {
        inherited_socket = socket(family, SOCK_STREAM, 0); check(inherited_socket, "parent socket");
        pid_t child = fork(); check(child, "fork");
        if (!child) return tcp(0, 1, 0);
        close(inherited_socket);
        int status; check(waitpid(child, &status, 0), "waitpid");
        return WIFEXITED(status) ? WEXITSTATUS(status) : 75;
    }
    upload_only = !strcmp(mode, "upload-exit");
    invalid_calls = !strcmp(mode, "invalid");
    if (invalid_calls) {
        int fd = socket(family, SOCK_STREAM, 0); check(fd, "invalid test socket");
        if (connect(fd, (void *)1, destination_size) != -1 || errno != EFAULT) return 68;
        if (connect(fd, destination, 1) != -1 || errno != EINVAL) return 67;
        if (connect(-1, destination, destination_size) != -1 || errno != EBADF) return 66;
        close(fd);
    }
    if (!strcmp(mode, "threads")) {
        pthread_t threads[4];
        for (int i = 0; i < 4; i++) if (pthread_create(&threads[i], NULL, thread_client, NULL)) return 78;
        for (int i = 0; i < 4; i++) { void *status; pthread_join(threads[i], &status); if (status) return (int)(long)status; }
        return 0;
    }
    if (!strcmp(mode, "background")) {
        pid_t child = fork(); check(child, "fork");
        if (child) return 23;
        usleep(200000);
        int fd = open("late-child.txt", O_CREAT | O_WRONLY, 0600); check(fd, "late open"); close(fd);
        return tcp(0, 1, 0);
    }
    if (!strcmp(mode, "udp")) {
        int fd = socket(AF_INET, SOCK_DGRAM, 0); check(fd, "udp socket");
        check(connect(fd, destination, destination_size), "udp connect");
        struct sockaddr_in peer; socklen_t size = sizeof(peer);
        check(getpeername(fd, (struct sockaddr *)&peer, &size), "udp peer");
        if (ntohs(peer.sin_port) != port) return 77;
        check(write(fd, "udp", 3), "udp send");
        char bytes[3]; if (read(fd, bytes, 3) != 3 || memcmp(bytes, "udp", 3)) return 76;
        close(fd); puts("UDP_OK"); return 0;
    }
    return tcp(!strcmp(mode, "nonblocking"), !strcmp(mode, "dup"), !strcmp(mode, "refused"));
}

static int signal_gate = -1;
static volatile sig_atomic_t signalled;
static void on_alarm(int signal) {
    (void)signal;
    signalled = 1;
    if (signal_gate >= 0 && write(signal_gate, "s", 1) != 1) _exit(74);
}

static int interrupted_opens(void) {
    check(mkfifo("fifo", 0600), "mkfifo");
    struct sigaction action = {.sa_handler = on_alarm};
    sigemptyset(&action.sa_mask);
    check(sigaction(SIGALRM, &action, NULL), "sigaction");
    ualarm(50000, 0);
    if (open("fifo", O_RDONLY) != -1 || errno != EINTR || !signalled) return 73;
    int gate[2]; check(pipe(gate), "signal pipe");
    pid_t child = fork(); check(child, "signal fork");
    if (!child) {
        close(gate[1]); char byte;
        if (read(gate[0], &byte, 1) != 1) _exit(72);
        int fd = open("fifo", O_WRONLY); check(fd, "fifo writer");
        check(write(fd, "x", 1), "fifo write"); close(fd); _exit(0);
    }
    close(gate[0]); signal_gate = gate[1];
    action.sa_flags = SA_RESTART;
    check(sigaction(SIGALRM, &action, NULL), "restart sigaction");
    ualarm(50000, 0);
    int fd = open("fifo", O_RDONLY); check(fd, "restarted open");
    char byte; if (read(fd, &byte, 1) != 1 || byte != 'x') return 71;
    close(fd); close(gate[1]); signal_gate = -1;
    int status; check(waitpid(child, &status, 0), "signal wait");
    if (!WIFEXITED(status) || WEXITSTATUS(status)) return 70;
    fd = open("after-signal.txt", O_CREAT | O_WRONLY, 0600); check(fd, "after signal"); close(fd);
    puts("SIGNALS_OK"); return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "files")) return files();
    if (argc == 2 && !strcmp(argv[1], "signals")) return interrupted_opens();
    if (argc == 3) return network(argv[1], atoi(argv[2]));
    return 90;
}
