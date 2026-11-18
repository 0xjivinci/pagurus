---
name: screenshot
description: Take a screenshot of a local web page/dashboard or of the Pi's screen and send it to the user
---

Use when the user wants to *see* the screen, a kiosk or a dashboard.

**A. The physical screen (Wayland desktops such as labwc)**
1. Needs approval: `XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 grim /tmp/shot.png`
   (1000 is the usual user id; `id -u` shows it.) Fast and exact, but black when the screen is off.

**B. A web page rendered headless (when the screen is off)**
1. Find the URL: the kiosk's command line (`ps -eo args | grep -- --kiosk`) or the service file.
2. Needs approval; 20–60 s on a Pi 3B:
   `chromium --headless=new --disable-gpu --hide-scrollbars --window-size=1280,800 --virtual-time-budget=5000 --user-data-dir=/tmp/shot-profile --screenshot=/tmp/shot.png URL`
   Pages that fade in content with JavaScript or draw with WebGL often come out black this way.

**Then**
3. Check it is not black: `ls -l /tmp/shot.png`. A whole-screen PNG under ~10 KB is a single colour, almost always
   black (screen off / power save). Then look at it with `view_image` (path `/tmp/shot.png`) if your model can see images: check it isn't blank and note
   what's on it. Without vision, check for a black frame instead:
   `python3 -c "from PIL import Image; print(Image.open('/tmp/shot.png').getextrema())"` (every channel `(0, 0)` = black).
   A black image means the screen is off or in power save: say so plainly and offer the other way, or the page's
   numbers via `web_fetch`, instead of retrying.
4. Send it with `send_file` (path `/tmp/shot.png`) and a one-line caption. **Always finish with either the sent
   image or a plain explanation of why there is none** (e.g. "the screen is in power save, so the shot is black;
   here are the dashboard's numbers instead"). Never end the turn with a screenshot taken but not sent.

**Pitfalls**
- Describe the image only from what `view_image` showed you; without vision, only from what commands told you.
- To *read* a dashboard's numbers, `web_fetch` on its URL (or its JSON endpoint) is faster and needs no approval.
- After two failed attempts, stop and report what happened instead of trying variations.
