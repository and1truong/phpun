#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stddef.h>

// ponytail: out-of-line libc memcpy/memmove only, not inline LLVM copies,
// hidden libc copies, memcpy implementations called by another symbol, or realloc.
// Single-thread CLI only (resolver pointers); not a server/thread profiler.
// Fixed table avoids allocator recursion; overflow is explicitly reported.
// Single-thread CLI only: lazy dlsym resolver pointers are not synchronized.
#define SLOTS 8192
struct bucket { atomic_uintptr_t pc; atomic_ullong calls, bytes; };
static struct bucket copies[SLOTS];
static atomic_ullong overflow_calls, overflow_bytes;
static _Thread_local int active;
static void *(*next_copy)(void *, const void *, size_t);
static void *(*next_move)(void *, const void *, size_t);

static void *fallback(void *dst, const void *src, size_t n) {
    volatile unsigned char *d = dst;
    const volatile unsigned char *s = src;
    if ((uintptr_t)d > (uintptr_t)s) {
        while (n) { n--; d[n] = s[n]; }
    } else for (size_t i = 0; i < n; i++) d[i] = s[i];
    return dst;
}
static void record(uintptr_t pc, size_t n) {
    size_t slot = (pc >> 3) % SLOTS;
    for (size_t i = 0; i < SLOTS; i++, slot = (slot + 1) % SLOTS) {
        uintptr_t old = atomic_load_explicit(&copies[slot].pc, memory_order_relaxed);
        if (!old) atomic_compare_exchange_strong(&copies[slot].pc, &old, pc);
        if (!old || old == pc) {
            atomic_fetch_add_explicit(&copies[slot].calls, 1, memory_order_relaxed);
            atomic_fetch_add_explicit(&copies[slot].bytes, n, memory_order_relaxed);
            return;
        }
    }
    atomic_fetch_add(&overflow_calls, 1);
    atomic_fetch_add(&overflow_bytes, n);
}
void *memcpy(void *dst, const void *src, size_t n) {
    if (active) return fallback(dst, src, n);
    active = 1;
    if (!next_copy) next_copy = dlsym(RTLD_NEXT, "memcpy");
    record((uintptr_t)__builtin_return_address(0), n);
    void *out = next_copy ? next_copy(dst, src, n) : fallback(dst, src, n);
    active = 0;
    return out;
}
void *memmove(void *dst, const void *src, size_t n) {
    if (active) return fallback(dst, src, n);
    active = 1;
    if (!next_move) next_move = dlsym(RTLD_NEXT, "memmove");
    record((uintptr_t)__builtin_return_address(0), n);
    void *out = next_move ? next_move(dst, src, n) : fallback(dst, src, n);
    active = 0;
    return out;
}
__attribute__((destructor)) static void report(void) {
    active = 1;
    for (size_t i = 0; i < SLOTS; i++) {
        uintptr_t pc = atomic_load(&copies[i].pc);
        if (!pc) continue;
        Dl_info info = {0};
        dladdr((void *)pc, &info);
        fprintf(stderr, "copy-profile\t%s\t%zx\t%llu\t%llu\n",
            info.dli_fname ? info.dli_fname : "?", pc - (uintptr_t)info.dli_fbase,
            atomic_load(&copies[i].calls), atomic_load(&copies[i].bytes));
    }
    fprintf(stderr, "copy-profile-overflow\t%llu\t%llu\n",
        atomic_load(&overflow_calls), atomic_load(&overflow_bytes));
}
