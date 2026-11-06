#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void check(long result, const char *operation) {
    if (result < 0) { perror(operation); exit(91); }
}

static int anonymous_mapping(void) {
    size_t length = (size_t)sysconf(_SC_PAGESIZE) * 3;
    // Linux ignores fd for anonymous mappings, even when it is not valid.
    unsigned char *bytes = mmap(NULL, length, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, 12345, 0);
    if (bytes == MAP_FAILED) { perror("anonymous mmap"); return 92; }
    if (bytes[0] || bytes[length - 1]) return 93;
    bytes[0] = 0x5a;
    bytes[length - 1] = 0xa5;
    if (bytes[0] != 0x5a || bytes[length - 1] != 0xa5) return 94;
    uintptr_t address = (uintptr_t)bytes;
    check(munmap(bytes, length), "anonymous munmap");
    printf("{\"address\":\"0x%" PRIxPTR "\",\"length\":%zu,\"pid\":%d,\"tid\":%ld}\n",
           address, length, getpid(), syscall(SYS_gettid));
    return 0;
}

static void *thread_mapping(void *unused) {
    (void)unused;
    return (void *)(intptr_t)anonymous_mapping();
}

static int file_mappings(void) {
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    const char *name = "mapped-\xff.bin";
    int fd = open(name, O_CREAT | O_RDWR | O_TRUNC, 0600);
    check(fd, "open mapped file");
    check(ftruncate(fd, (off_t)(page * 2)), "truncate mapped file");
    if (pwrite(fd, "original", 8, (off_t)page) != 8) return 95;
    char *shared = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)page);
    if (shared == MAP_FAILED) { perror("shared mmap"); return 96; }
    char *private = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, (off_t)page);
    if (private == MAP_FAILED) { perror("private mmap"); return 97; }
    int mapped_fd = fd;
    check(close(fd), "close mapped fd");
    if (memcmp(shared, "original", 8) || memcmp(private, "original", 8)) return 98;
    private[0] = 'P';
    shared[0] = 'S';
    check(msync(shared, page, MS_SYNC), "sync shared mapping");
    if (private[0] != 'P' || shared[0] != 'S') return 99;
    uintptr_t shared_address = (uintptr_t)shared, private_address = (uintptr_t)private;
    check(munmap(shared, page), "unmap shared");
    check(munmap(private, page), "unmap private");
    fd = open(name, O_RDONLY);
    check(fd, "reopen mapped file");
    char contents[8];
    if (pread(fd, contents, sizeof(contents), (off_t)page) != 8 || memcmp(contents, "Sriginal", 8)) return 100;
    check(close(fd), "close reopened file");
    printf("{\"shared_address\":\"0x%" PRIxPTR "\",\"private_address\":\"0x%" PRIxPTR "\",\"fd\":%d,\"page\":%zu,\"pid\":%d,\"tid\":%ld}\n",
           shared_address, private_address, mapped_fd, page, getpid(), syscall(SYS_gettid));
    return 0;
}

static int failures(void) {
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    int fd = open("mmap-errors.bin", O_CREAT | O_RDWR | O_TRUNC, 0600);
    check(fd, "open failure fixture");
    check(ftruncate(fd, (off_t)page), "truncate failure fixture");
    errno = 0;
    if (mmap(NULL, 0, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) != MAP_FAILED || errno != EINVAL) return 101;
    errno = 0;
    if (mmap(NULL, page, PROT_READ, MAP_PRIVATE, -1, 0) != MAP_FAILED || errno != EBADF) return 102;
    errno = 0;
    // libc rejects unaligned offsets before entering the kernel; exercise the
    // native syscall so this assertion also has an observable ptrace stop.
    if (syscall(SYS_mmap, NULL, page, PROT_READ, MAP_PRIVATE, fd, (off_t)1) != -1 || errno != EINVAL) return 103;
    check(close(fd), "close failure fixture");
    printf("{\"fd\":%d,\"page\":%zu,\"pid\":%d,\"tid\":%ld}\n",
           fd, page, getpid(), syscall(SYS_gettid));
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2) return 90;
    if (!strcmp(argv[1], "anonymous")) return anonymous_mapping();
    if (!strcmp(argv[1], "file")) return file_mappings();
    if (!strcmp(argv[1], "failures")) return failures();
    if (!strcmp(argv[1], "thread")) {
        pthread_t thread;
        int error = pthread_create(&thread, NULL, thread_mapping, NULL);
        if (error) { errno = error; check(-1, "pthread_create"); }
        void *status;
        error = pthread_join(thread, &status);
        if (error) { errno = error; check(-1, "pthread_join"); }
        return (int)(intptr_t)status;
    }
    if (!strcmp(argv[1], "fork")) {
        pid_t child = fork();
        check(child, "fork");
        if (!child) return anonymous_mapping();
        int status;
        check(waitpid(child, &status, 0), "waitpid");
        return WIFEXITED(status) ? WEXITSTATUS(status) : 104;
    }
    return 90;
}
