/* Minimal shim for SAI's legacy <sys/types.h> include.
 *
 * The SAI headers only need size_t (via sai_size_t) and the stdint types,
 * all of which come from clang's builtin headers. Using this shim instead
 * of the host libc keeps bindgen output byte-identical on every build host
 * (bindings always target x86_64-unknown-linux-gnu). */
#pragma once
#include <stddef.h>
#include <stdint.h>
