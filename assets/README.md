# Fonts and icons

All fonts are under the SIL Open Font License (`OFL-*.txt`).

| File | Font | Used for |
| --- | --- | --- |
| `BarlowCondensed-*.ttf` | Barlow Condensed | headings, keys (and the macOS disk image's artwork) |
| `IBMPlexMono-Regular.ttf` | IBM Plex Mono | the log |
| `ShareTechMono-Regular.ttf` | Share Tech Mono | labels |
| `VT323-Regular.ttf` | VT323 | the LCD |
| `SairaExpanded-BlackItalic-wordmark.ttf` | Saira | the "M-FLASHER" wordmark (`src/wordmark.rs`) and the icon's M |

The wordmark font is the variable Saira Italic from Google Fonts
(`ofl/saira/Saira-Italic[wdth,wght].ttf`) as a static Expanded Black instance,
with the overlapping contours merged (they show when the letters are drawn in
outline) and only the wordmark's letters kept, with fontTools and skia-pathops:

```sh
fonttools varLib.instancer 'Saira-Italic[wdth,wght].ttf' wdth=125 wght=900 --remove-overlaps -o SairaExpanded-BlackItalic.ttf
pyftsubset SairaExpanded-BlackItalic.ttf --text=M-FLASHER --layout-features= --output-file=SairaExpanded-BlackItalic-wordmark.ttf
```

`icon-256.png` is the window's icon on macOS; `../macos/render.sh` makes it,
`../linux/m-flasher.png` and `../windows/m-flasher.ico` from `../macos/icon.svg`.
