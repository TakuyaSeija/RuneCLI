# Brand originals

The owner's logo, supplied 2026-10-31 as three 256 px renders of the same mark.

| File | What it is | Used for |
| --- | --- | --- |
| `Logo-transparent.png` | the mark alone, transparent ground | the header and footer mark (`Images/Logo.png`, 256 px), the browser tab icons (`Images/Icon-128.png` 128 px, `Icon.png` 96 px, `Icon-32.png` 32 px, `Icon-16.png` 16 px) and the mark on the home-screen icon (`Images/AppleTouchIcon.png`, 180 px on the dark plate) |
| `Logo-black.png` | the mark on the dark plate | kept as a brand original; the site and tabs use the transparent mark |
| `Logo-white.png` | the mark on the light plate | kept for a light surface; nothing uses it today, because the site is dark-only |

Never edit these files and never re-colour the mark: every served file is
derived from them by `Tools/build_brand_assets.py`, and
`python3 -I Tools/build_brand_assets.py --check` proves the derived files are
exactly its output. `Server/Brand_test.go` holds their sizes, transparency and
the markup that names them.

Why the browser tab icons use the transparent mark: the transparent mark blends
cleanly and seamlessly with the browser chrome without introducing an artificial
dark plate box around the mark. Four crisp Lanczos downscales (128, 96, 32, and
16 px) ensure high-density displays (Retina, 4K, 5K) render the swirl in full HD
sharpness. The iOS home-screen icon (`AppleTouchIcon.png`) retains the dark plate
background (`#0f1115`) because iOS rounds corners and fills transparent areas
with solid black on the home screen.
