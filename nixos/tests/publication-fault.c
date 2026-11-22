/* Test-VM-only interposer. Never linked into or installed with the appliance. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int armed(const char *source, const char *phase) {
    char wanted[32], key[65];
    FILE *file = fopen("/run/nasty-publication-fault", "r");
    if (!file) return 0;
    int fields = fscanf(file, "%31s %64s", wanted, key);
    fclose(file);
    return fields == 2 && strlen(key) == 64 && strcmp(wanted, phase) == 0
        && strstr(source, "/.nasty/provisioning/") && strstr(source, key);
}

static void pause_at(const char *phase, const char *destination) {
    int fd = open("/run/nasty-publication-fault-hit", O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    if (fd < 0) _exit(190);
    dprintf(fd, "%s %s %ld\n", phase, destination, (long)getpid());
    if (fsync(fd) != 0 || close(fd) != 0) _exit(191);
    raise(SIGSTOP); /* Test harness kills the stopped service, not resumes it. */
}

int renameat2(int olddirfd, const char *source, int newdirfd, const char *destination, unsigned int flags) {
    typedef int (*rename_fn)(int, const char *, int, const char *, unsigned int);
    rename_fn real_rename = (rename_fn)dlsym(RTLD_NEXT, "renameat2");
    if (!real_rename) { errno = ENOSYS; return -1; }
    int target = strstr(destination, "/atomic-crash-") != NULL;
    if (target && armed(source, "before-rename")) pause_at("before-rename", destination);
    int result = real_rename(olddirfd, source, newdirfd, destination, flags);
    int saved_errno = errno;
    if (result == 0 && target && armed(source, "after-rename")) pause_at("after-rename", destination);
    errno = saved_errno;
    return result;
}
