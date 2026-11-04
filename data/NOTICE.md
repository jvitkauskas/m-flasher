# Bundled files

From [m-bootloader](https://github.com/jvitkauskas/m-bootloader), MIT License,
(c) 2026 Julius Vitkauskas. The bootloaders are build `1.4-ac26c70` of
[pull request #6](https://github.com/jvitkauskas/m-bootloader/pull/6), not a
release yet, and the flash loader is unchanged from v1.3:

| File | What | Release file |
| --- | --- | --- |
| `bootloader-fm1.boot` | m-bootloader for the FM-1 | `fm1-uboot.boot` |
| `bootloader-smk37.boot` | m-bootloader for the SMK-37 Pro / Elite | `smk37-uboot.boot` |
| `mflr-loader-a.bin`, `mflr-loader-b.bin` | the flash loader M-Flasher runs on the chip, for RAM `0x01C20000` and `0x01C28000` | the same names |

Replace them with the release's files, byte-identical (its `SHA256SUMS`), before
this is merged.
