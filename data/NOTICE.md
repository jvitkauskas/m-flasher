# Bundled files

From [m-bootloader](https://github.com/jvitkauskas/m-bootloader), MIT License,
(c) 2026 Julius Vitkauskas:

| File | What | Release file |
| --- | --- | --- |
| `bootloader-fm1.boot` | m-bootloader for the FM-1 | `fm1-uboot.boot`, [v2.4](https://github.com/jvitkauskas/m-bootloader/releases/tag/v2.4) |
| `bootloader-smk37.boot` | m-bootloader for the SMK-37 Pro / Elite | `smk37-uboot.boot`, [v2.4](https://github.com/jvitkauskas/m-bootloader/releases/tag/v2.4) |
| `mflr-loader-a.bin`, `mflr-loader-b.bin` | the flash loader M-Flasher runs on the chip, for RAM `0x01C20000` and `0x01C28000` | the same names, [v2.3](https://github.com/jvitkauskas/m-bootloader/releases/tag/v2.3) |

The files are byte-identical to those releases' (their `SHA256SUMS`). The flash
loader stays at v2.3's build: v2.4 rebuilt the same code with a newer compiler,
and that build has not been run on a unit yet.
