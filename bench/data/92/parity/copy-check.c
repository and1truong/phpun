#include <string.h>
#include <stdio.h>
int main(void) {
    char a[256],b[256];
    memset(a, 'x', sizeof(a));
    memcpy(b, a, sizeof(a));
    memmove(b + 1, b, 128);
    for (int i = 0; i < 256; i++) if (b[i] != 'x') return 1;
    puts("copy-ok");
}
