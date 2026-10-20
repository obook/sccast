# Cast Streaming (`--format mirror`)

Tested on one TV only: Sharp, Android TV, 2026-10-20.

## Result

The TV shows the screen 153 ms after the PC (one photo, 1080p30,
2500 kbit/s). HLS: 2 to 3 s. Reading ms on a photo is rough: about 20 ms of
error.

## How it works

- sccast launches Google's mirroring receiver (`0F5096E8`) and sends an OFFER
  on `urn:x-cast:com.google.cast.webrtc`. The TV answers with a UDP port.
- No sender check: any program can do it, not only Chrome.
- H.264 and Opus frames are encrypted (AES-128-CTR) and sent over RTP.
- A Sender Report every 100 ms lets the TV time the frames.
- The TV asks for lost packets (sccast resends them) and for key frames.
- The bitrate goes down when the TV reports losses, back up when they stop.

Protocol taken from openscreen (`cast/streaming/impl`). openscreen's own
`cast_sender` was not built: it needs libvpx, a Chromium toolchain, and
only reads files.

## Code

- `src/mirror.rs`: the sender.
- `src/capture.rs`: same capture as HLS, ending in `appsink`.
- `proto/`: the Python prototype used to check the protocol first.

## Measures on the Sharp

| | |
|---|---|
| Delay, real screen | 153 ms |
| Delay, Python prototype, test pattern | 133 ms |
| Resent packets | about 5 per second, no cut |
| Ctrl+C | TV freed |

## Not done

- Other devices (Chromecast, Nest Hub).
- Menu tests: Enter, `s`, `q`, closing the terminal.
- Reconnection when the TV drops the session, and launch retries in the menu.
- The OFFER goes to the broadcast address `*` (rust_cast has no other way):
  checked on the Sharp only.
- Lip sync measure (the user saw maybe a slight offset).
- Tuning: target delay fixed at 100 ms, bitrate steps are guesses.
