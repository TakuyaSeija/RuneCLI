# Brand originals

The owner's logo, supplied 2026-10-31 as three 256 px renders of the same mark.

| File | What it is | Used for |
| --- | --- | --- |
| `Logo-transparent.png` | the mark alone, transparent ground | the header and footer mark (`Images/Logo.png`, 96 px) and the home-screen icon (`Images/AppleTouchIcon.png`, 180 px on the dark plate) |
| `Logo-black.png` | the mark on the dark plate | the browser tab icons (`Images/Icon.png` 96 px, `Icon-32.png`, `Icon-16.png`) |
| `Logo-white.png` | the mark on the light plate | kept for a light surface; nothing uses it today, because the site is dark-only |

Never edit these files and never re-colour the mark: every served file is
derived from them by `Tools/build_brand_assets.py`, and
`python3 -I Tools/build_brand_assets.py --check` proves the derived files are
exactly its output. `Server/Brand_test.go` holds their sizes, transparency and
the markup that names them.

Why the tab icon is the dark plate and not the transparent mark: a gold swirl
on transparency loses its edge at 16 px and disappears on a light tab; the
owner's own dark plate keeps it legible on both. The header and footer use the
transparent mark, because a plate there reads as a badge stuck onto the page.
