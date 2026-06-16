/*
 * libc++ `__libcpp_verbose_abort` fallback (Linux only).
 *
 * Zig 0.15 compiles ghostty's C++ against a recent LLVM libc++ whose
 * headers inline `std::__throw_length_error` (and friends) into a call to
 * `std::__1::__libcpp_verbose_abort` when the code is optimized. The Debug
 * build kept an out-of-line `__throw_length_error` call instead, so the
 * symbol never surfaced — switching the vendored library to an optimized
 * build (`-Doptimize=ReleaseSafe`) does.
 *
 * ghostty's static archive does NOT bundle libc++; consumers link the
 * system `-lc++`. On ubuntu-22.04 that runtime is LLVM 14, which predates
 * `__libcpp_verbose_abort`, so the optimized archive fails to link there.
 *
 * Provide a WEAK fallback so a newer libc++ that does export the symbol
 * still wins. The function is `noreturn` and these call sites are the
 * unreachable container-overflow paths, so aborting matches libc++'s own
 * behavior (its real implementation prints a message and aborts).
 *
 * The asm label is the Itanium-mangled name of
 * `std::__1::__libcpp_verbose_abort(char const*, ...)` (ELF, no leading
 * underscore). macOS doesn't need this shim — its SDK libc++ has the
 * symbol — and its Mach-O underscore prefix differs, so build.rs compiles
 * this only for Linux targets.
 */

#include <stdlib.h>

__attribute__((noreturn, weak)) void lazybox_libcpp_verbose_abort(const char *fmt, ...)
    __asm__("_ZNSt3__122__libcpp_verbose_abortEPKcz");

void lazybox_libcpp_verbose_abort(const char *fmt, ...) {
    (void)fmt;
    abort();
}
