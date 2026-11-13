#!/usr/bin/env bash
# Builds target/ps5/kraken-mount.elf, the PS5 payload, in Docker (toolchain from ps5-dump-forge).
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" != --inside ]]; then
  docker build -t kraken-mount-ps5 ps5
  exec docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -e CARGO_HOME=/work/target/ps5/cargo -v "$PWD:/work" kraken-mount-ps5 ps5/build.sh --inside
fi

# The bundled ampr-emu-ng must be the released file (SHA256SUMS copied from its GitHub release).
(cd vendor/ampr-emu-ng && sha256sum -c --ignore-missing --strict SHA256SUMS)

out=target/ps5
cc="$PS5_PAYLOAD_SDK/bin/prospero-clang"
mkdir -p "$out/shim"
for src in ps5/shim/freebsd11.c ps5/shim/syscalls.S ps5/entry.c ps5/compat.c; do
  name=$(basename "${src%.*}")
  "$cc" -O2 -Wall -Wextra -Werror -c "$src" -o "$out/shim/$name.o"
done

export RUSTC_BOOTSTRAP=1
# The PS5 kernel fills FreeBSD 11 structs (stat 120 bytes, dirent 264, old statfs); libc and the
# std built here default to FreeBSD 12's. libc >= 0.2.187 reads this cfg; rustflags reach std
# too under -Z build-std.
export CARGO_TARGET_X86_64_PS5_FREEBSD_RUSTFLAGS='--cfg libc_unstable_freebsd_version="11"'
export CARGO_TARGET_DIR="$out"
export CARGO_TARGET_X86_64_PS5_FREEBSD_LINKER=/work/ps5/linker.sh
export CC_x86_64_ps5_freebsd="$cc" AR_x86_64_ps5_freebsd="$PS5_PAYLOAD_SDK/bin/prospero-ar"
# Cargo doesn't see the C objects or linker.sh: drop the linked output so a shim change relinks.
build() {
  rm -f "$out"/x86_64-ps5-freebsd/release/deps/kraken_mount-*
  cargo build --release --locked \
    --target ps5/x86_64-ps5-freebsd.json -Z json-target-spec \
    -Z build-std=std,panic_abort -Z build-std-features=panic-unwind "$@"
}
# Stage 1 is the copy the tile starts (/data/krakenmount/kraken-mount.elf); stage 2, the ELF you
# push, embeds it and saves it there (as Dump Forge's self copy).
build
cp "$out/x86_64-ps5-freebsd/release/kraken-mount" "$out/kraken-mount-stage1.elf"
KRAKEN_SELF_ELF="/work/$out/kraken-mount-stage1.elf" build --features self-copy
cp "$out/x86_64-ps5-freebsd/release/kraken-mount" "$out/kraken-mount.elf"
llvm-readelf -h -d "$out/kraken-mount.elf" > "$out/kraken-mount.elf.txt"
ls -l "$out/kraken-mount.elf"
