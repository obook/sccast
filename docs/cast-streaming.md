# Screen mirroring (`--format mirror`, the default)

Tested on the Sharp TV (Android TV), 2026-10-20. A Nest Hub accepted a
session once, video and sound, for 5 s: not tested further.

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
- The TV asks for lost packets and for key frames. sccast resends a lost
  packet at most every 50 ms, and forces a key frame at most once a second.
- The bitrate goes down by 20 % when the TV reports losses (at most once a
  second), and back up by 10 % after 10 s without loss.
- On a device without the mirroring receiver, sccast casts in HLS instead.

Protocol taken from openscreen (`cast/streaming/impl`). openscreen's own
`cast_sender` was not built: it needs libvpx, a Chromium toolchain, and
only reads files.

## Code

- `src/mirror/`: the sender. `offer.rs` opens the session, `send.rs` runs
  the threads, `rtp.rs` builds the packets, `rtcp.rs` reads the TV's
  reports, `rate.rs` adapts the bitrate.
- `src/capture/`: same capture as HLS, ending in `appsink`.
- `proto/`: the Python prototype used to check the protocol first.

## Measures on the Sharp

| | |
|---|---|
| Delay, real screen | 153 ms |
| Delay, Python prototype, test pattern | 133 ms |
| Resent packets | about 5 per second, no cut |
| Ctrl+C, `s`, `q`, closing the terminal | TV freed |

## Not done

- Other devices: the school Chromecast, a Nest Hub beyond the 5 s above.
- The HLS fallback on a device without mirroring.
- A firewall on the PC (ufw, firewalld).
- Reconnection when the TV drops the session.
- The OFFER goes to the broadcast address `*` (rust_cast has no other way):
  checked on the Sharp and one Nest Hub only.
- Lip sync measure (the user saw maybe a slight offset).
- Tuning: target delay fixed at 100 ms, bitrate steps are guesses.
