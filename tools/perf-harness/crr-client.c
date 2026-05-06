// crr-client.c — N-iteration TCP CRR loop inside a single process.
//
// Usage: crr-client HOST PORT N
// Output: one line "n p50_ns p99_ns mean_ns" to stdout.
//
// Each iteration: socket → connect → write 1 byte → read 1 byte → close.
// Times the full cycle with CLOCK_MONOTONIC.  No fork, no exec, no
// per-iteration interpreter overhead — isolates the user-mode TCP /
// NAT path from the bench's outer process-spawning loop.

#include <arpa/inet.h>
#include <errno.h>
#include <limits.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static int cmp_long(const void *a, const void *b) {
    long la = *(const long *)a, lb = *(const long *)b;
    return (la > lb) - (la < lb);
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: %s HOST PORT N\n", argv[0]);
        return 1;
    }
    const char *host = argv[1];
    int port = atoi(argv[2]);
    int n = atoi(argv[3]);
    if (n <= 0 || n > 1000000) {
        fprintf(stderr, "N out of range\n");
        return 1;
    }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    if (inet_pton(AF_INET, host, &addr.sin_addr) != 1) {
        fprintf(stderr, "bad host %s\n", host);
        return 1;
    }

    long *samples = calloc((size_t)n, sizeof(long));
    if (!samples) return 2;

    for (int i = 0; i < n; i++) {
        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);

        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0) { perror("socket"); return 3; }
        if (connect(fd, (struct sockaddr *)&addr, sizeof addr) < 0) {
            perror("connect");
            return 3;
        }
        ssize_t w = write(fd, "y", 1);
        (void)w;
        char buf;
        ssize_t r = read(fd, &buf, 1);
        (void)r;
        close(fd);

        clock_gettime(CLOCK_MONOTONIC, &t1);
        long ns = (t1.tv_sec - t0.tv_sec) * 1000000000L
                + (t1.tv_nsec - t0.tv_nsec);
        samples[i] = ns;
    }

    qsort(samples, (size_t)n, sizeof(long), cmp_long);
    long sum = 0;
    for (int i = 0; i < n; i++) sum += samples[i];
    long p50  = samples[n / 2];
    long p99  = samples[(n * 99) / 100];
    long mean = sum / n;
    printf("%d %ld %ld %ld\n", n, p50, p99, mean);

    free(samples);
    return 0;
}
