#include <string.h>
#include <stdio.h>
// Volatile indirect calls stay opaque even when GCC folds initialization/copies.
static void *(*volatile copy_call)(void *, const void *, size_t) = memcpy;
static void *(*volatile move_call)(void *, const void *, size_t) = memmove;
int main(void) {
    char a[256],b[256];
    memset(a, 'x', sizeof(a));
    copy_call(b, a, sizeof(a));
    move_call(b + 1, b, 128);
    for (int i = 0; i < 256; i++) if (b[i] != 'x') return 1;
    puts("copy-ok");
}
