#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void) {
    int fd = open("example.json", O_RDONLY);
    if (fd < 0 || lseek(fd, 0, SEEK_CUR) != 0) return 1;
    if (rename("example.json", "original.json")) return 2;
    int replacement = open("example.json", O_WRONLY | O_CREAT, 0600);
    if (replacement < 0 || write(replacement, "replacement", 11) != 11) return 3;
    close(replacement);
    char buffer[64] = {0};
    ssize_t length = read(fd, buffer, sizeof(buffer));
    if (length != 13 || memcmp(buffer, "{\"ok\":true}\n\n", 13)) return 4;
    close(fd);
    if (unlink("original.json")) return 5;
    fd = open("empty.txt", O_RDONLY);
    if (fd < 0) return 6;
    close(fd);
    fd = open("binary.bin", O_RDONLY);
    if (fd < 0) return 7;
    close(fd);
    fd = open("/dev/null", O_RDONLY);
    if (fd < 0) return 8;
    close(fd);
    fd = open("/proc/self/stat", O_RDONLY);
    if (fd < 0) return 9;
    close(fd);
    fd = open(".", O_RDONLY | O_DIRECTORY);
    if (fd < 0) return 10;
    close(fd);
    if (open("does-not-exist", O_RDONLY) >= 0) return 11;
    puts("FILE_PREVIEW_OK");
    return 0;
}
