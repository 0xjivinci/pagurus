---
name: make-a-document
description: Make a polished PDF or HTML document (report, summary, art findings) with headings, tables and images, and send it to the user.
---

Use when the user asks for a PDF, a document, a printable report, or "put this in a file".

**Artworks (Tezos):** don't build it by hand. Call `art` with `action=report` and `pieces` (objkt links) and/or `who` (artists, comma separated; `per` works each). It downloads the images, embeds them with the market numbers (mint date, edition, sold out or open, floor, sales, volume, last sale) and attaches the PDF itself. Find the artists first with `art` (style, trending, similar, search); never put artists or numbers in a report that a tool didn't return.

## Steps
1. Gather the content first (research, numbers, image files). Images: use files you already have in the workspace (e.g. `workspace/art/` from the art tool's look); don't download them again.
2. Write one self-contained HTML file in the workspace with `write_file`:
   - simple CSS in a `<style>` tag: readable font, max-width ~800px, tables with borders, images `max-width:100%`
   - a title, the date, then sections; captions under images
   - images as files you downloaded into the workspace (relative paths) or data URLs; never remote `https://`/`ipfs://` links, since headless Chromium often can't fetch them and the PDF comes out blank
3. Turn it into a PDF with the first converter that exists (`which` it first):
   - `timeout 120 chromium --headless --disable-gpu --no-pdf-header-footer --user-data-dir=/tmp/pdf-profile --print-to-pdf=out.pdf file.html` (its own profile, so it doesn't attach to a running browser and hang) (also `chromium-browser`, `google-chrome`, or on a Mac `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` / Brave)
   - `wkhtmltopdf file.html out.pdf`
   - `pandoc file.html -o out.pdf`
   If none exists, send the HTML file instead and say why.
4. Check the PDF exists and isn't tiny (`ls -l`; a few KB means the images are missing), then `send_file` it.
5. Say in one line what's in it.

## Rules
- Only say a file was sent after `send_file` succeeded.
- Keep facts and numbers exactly as you found them; cite sources at the end.
- Big images make huge PDFs: prefer previews over full-size originals.
