#!/usr/bin/env bash
# rustc's linker for the PS5 target (from ps5-dump-forge). FreeBSD splits these symbols into
# separate libraries; the PS5 SDK has them in libc and Sony's modules. A missing function still
# fails the link: no unresolved-symbol suppression.
set -euo pipefail
args=()
for arg in "$@"; do
  case "$arg" in
    -lm|-lrt|-lutil|-lexecinfo|-lkvm|-lmemstat|-lprocstat|-ldevstat) args+=(-lc) ;;
    -lgcc_s) args+=(-lunwind) ;;
    *) args+=("$arg") ;;
  esac
done
obj=/work/target/ps5/shim
mapfile -t wraps < /work/ps5/shim/syscalls.link
# rustc passes -nodefaultlibs, so the SDK's native imports are named here.
exec "$PS5_PAYLOAD_SDK/bin/prospero-clang" "${args[@]}" \
  "$obj/freebsd11.o" "$obj/syscalls.o" "$obj/entry.o" "$obj/compat.o" "${wraps[@]}" \
  -Wl,--wrap=main -Wl,--wrap=fcntl -Wl,--wrap=cpuset_getaffinity -Wl,--error-limit=0 \
  -ldl -lunwind -lc -lkernel_web -lSceLibcInternal -lSceNet -lSceSystemService -lSceUserService -lSceNotification -lSceAppInstUtil
