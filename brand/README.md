# Brand

The xolotl emblem is a canine in a portal: Xolotl, the dog-headed guide through the underworld, standing in a gateway.

## Files

| File | Use |
|---|---|
| `xolotl-emblem.svg` | The master. Ink and rust on a transparent background, cropped to the artwork. |
| `xolotl-emblem-dark.svg` | The same emblem with light ink, for dark backgrounds. |
| `xolotl-icon.svg`, `xolotl-icon-dark.svg` | The emblem on a square canvas with breathing room, for icons. |
| `xolotl-emblem.png`, `xolotl-social.png` | Rasters for the README and the repository's social preview. |
| `build.sh` | Regenerates the rasters here and the icons in `ui/public` from the SVGs, and copies the icons and `ui/public/favicon.svg` to `docs/user/assets` as the published logo, dark logo and favicon. Needs `agent-svgtools` and ImageMagick. |

The web UI draws the emblem inline from `ui/src/components/emblem.tsx`, with the ink in `currentColor` and the portal in the UI's primary colour. Its geometry is a copy of the master; change both together.

## Colours

The UI palette takes its ink and rust from the emblem.

| Name | Hex | Where |
|---|---|---|
| Ink | `#192029` | Canine, UI foreground and sidebar |
| Rust | `#b2411a` | Portal, UI primary and focus ring |
| Paper | `#f4f5f7` | Ink on dark backgrounds, UI page background |

## Construction

The ink is three stroked centrelines (ear and cheek, ear bar and muzzle, jaw) at a 32-unit stroke plus five filled shapes (second ear, eye, nose, throat, mane). The portal is an arch and two mirrored pillars, knocked out by a mask where the ink passes within 30 units, so the gap between ink and rust stays transparent on any background. Coordinates are in the 1254-unit space of the original artwork.
