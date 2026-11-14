# Phone embed preview

Build from client/ with `trunk build --public-url ./`, then run from the repo root:

    python3 tools/mobile-preview/serve.py

In another terminal:

    ngrok http http://127.0.0.1:8090 --inspect=false

Open the HTTPS tunnel URL on your phone. On the free tier, tap Visit Site once
if ngrok asks, then Run game. Keep both processes running during the test.
JS, WASM, and assets use the same HTTPS origin, correct MIME types, and gzip.
There is no live-reload WebSocket or local backend dependency.

The game uses demo data (?sandbox) with a touch-accessible Test screen menu.
Try join, prompting, failed prompt, waiting, countdown, fight, and results.
Type, dismiss the keyboard, send a prompt, open help and room settings, and rotate
both ways. Run in embedded view keeps the iframe inside the page; Run game
expands to the full window without locking orientation.

This approximates itch's iframe launch. There is deliberately no parent keyboard
or visual viewport bridge into the game. The harness shares the host origin;
actual itch permissions, keyboard, browser chrome, and fullscreen behavior still
need a final test on itch.
