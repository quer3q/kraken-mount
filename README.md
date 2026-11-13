<p align="center"><img src="docs/logo.png" width="128" alt="Kraken Mount"></p>

A PS5 ELF payload. It finds games on the internal SSD and on USB or extended drives.

It mounts debug FPKG `.pkg` files, game folders (plain, or packed with zstd or LZ4).

## How to

1. [ampr-emu-ng](https://github.com/quer3q/ampr-emu-ng) comes bundled and is installed into
   `/data/krakenmount/ampr/` at start. To run another build, put its `libSceAmpr.sprx` there; it is
   kept until you delete it.
2. Put each game in a scan root: `/data/homebrew`, `/data/etaHEN/games`, or `homebrew`,
   `etaHEN/games` or the root of a USB drive. A `.pkg` must be a plaintext debug FPKG from
   dump-forge. A folder needs `eboot.bin` and `sce_sys/param.json`. Backports go in
   `<scan root>/backports/<TITLE_ID>*`.
3. Download `kraken-mount.elf` from [Releases](https://github.com/quer3q/kraken-mount/releases)
   and send it once to the ELF loader (elfldr, port 9021):
   `nc -w 3 <ps5-ip> 9021 < kraken-mount.elf`. It saves a copy on the console, and from then on
   the Kraken Mount tile on the home screen starts it (elfldr must be running).
4. Each game gets a "<name> registered" notification and its own home-screen tile.
5. Open the Kraken Mount tile, or `http://<ps5-ip>:8097/` from a computer, to see the games,
   rebuild or delete them, change settings and read the log.

Caches live in `/data/krakenmount/caches/<TITLE_ID>`, settings in
`/data/krakenmount/config.toml` and the log in `/data/krakenmount/kraken.log`.

To build it yourself: `ps5/build.sh` (needs Docker), output `target/ps5/kraken-mount.elf`.

## Thanks

- [drakmor/ShadowMountPlus](https://github.com/drakmor/ShadowMountPlus): home-screen
  registration, the fakelib sandbox overlay and backports are ported from it.
- [ps5-payload-sdk](https://github.com/ps5-payload-dev/sdk): the PS5 toolchain.

## License

GPL-3.0-or-later (`LICENSE`). Third-party notices are in `THIRD_PARTY_NOTICES`.
