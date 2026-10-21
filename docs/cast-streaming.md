# Screen mirroring (`--format mirror`, the default)

Tested on the Sharp TV (Android TV), 2026-10-20, and a Nest Hub,
2026-10-21.

The Nest Hub takes 1080p and scales it down itself. Its ANSWER gives its
screen (`display.dimensions`: 1280x720, 30 fps, `scaling: receiver`) and
no `rtpExtensions`: its playout delay stays at 100 ms.

## Result

The TV shows the screen 153 ms after the PC (one photo, 1080p30,
2500 kbit/s). HLS: 2 to 3 s. Reading ms on a photo is rough: about 20 ms of
error.

## How it works

- sccast launches Google's mirroring receiver (`0F5096E8`) and sends an OFFER
  on `urn:x-cast:com.google.cast.webrtc`. The TV answers with a UDP port.
  If it refuses the video, sccast sends a second OFFER at 1280x720, then
  gives up. The whole ANSWER goes to the log.
- If the ANSWER gives a screen smaller than the video
  (`display.dimensions`), sccast encodes at the screen size, without a new
  OFFER. The menu shows that size, and goes back to the user's resolution
  at the next cast.
- No sender check: any program can do it, not only Chrome.
- H.264 and Opus frames are encrypted (AES-128-CTR) and sent over RTP.
- A Sender Report every 100 ms lets the TV time the frames.
- The TV asks for lost packets and for key frames. sccast resends a lost
  packet at most once per round trip, and never twice within 50 ms, and
  forces a key frame at most once a second.
- The round trip comes from the TV's Receiver Reports, smoothed as TCP does.
  Frames the TV acknowledges leave the resend cache.
- The bitrate goes down by 20 % when the TV reports losses (at most once a
  second), and back up by 10 % after 10 s without loss.
- The playout delay starts at 100 ms. It goes up one step (200, 300,
  400 ms) when the TV reports more than 2 % of packets lost or asks for a
  key frame, at most every 2 s, and down one step after 30 s without
  trouble. It travels in an RTP extension on the first packet of the frame
  after a change, the same on both streams, if the ANSWER accepts
  `adaptive_playout_delay`, as openscreen does.
  Otherwise it stays at 100 ms. The Sharp applies it (stopwatch photos
  below), though its feedback keeps reporting the OFFER's 100 ms.
- A session is lost when the control connection drops, the TV closes it, or
  no RTCP comes for 5 s (10 s before the first). sccast then launches the
  mirroring receiver again and sends a new OFFER: up to 3 times in 5 min
  from the terminal, without limit from the menu. A closed app (remote,
  another sender) or a capture failure ends the cast.
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
| TV network cut for 20 s (2026-10-21) | "Reconnecting" shown, stream back once plugged in |
| Still screen for 30 s (2026-10-21) | no reconnection |
| Delay with the extension forced at 100 ms (stopwatch photo) | about 180 ms |
| Delay with the extension forced at 400 ms (2 photos) | about 500 and 520 ms |
| Resent packets, 5 % loss with `tc netem` on the PC | 12 to 16 per second |
| Resent packets after removing it | 3 to 5 per second |
| Wi-Fi loss bursts of 8 to 11 % without netem | delay up to 400 ms, bitrate down to 1280 kbit/s, back up when calm |

## Not done

- Other devices: the school Chromecast.
- The HLS fallback on a device without mirroring.
- A firewall on the PC (ufw, firewalld).
- The OFFER goes to the broadcast address `*` (rust_cast has no other way):
  checked on the Sharp and one Nest Hub only.
- Lip sync: sound about 250 ms late on the Sharp, by eye (2026-10-21). Both
  streams leave the PC fresh (audio 20 ms old, video 30 to 65 ms). A
  negative audio delay only helps within the playout delay (100 ms): the TV
  cannot play a sound before it arrives. The source was the Freebox stream
  in VLC, whose sound is already slightly late on the PC: part of the
  250 ms comes from it. To measure on a page that flashes and beeps at once,
  filmed in slow motion.
- Until the TV acknowledges a video frame, sccast forces a key frame every
  second. The Sharp acknowledged the first one after 210 ms.
- Tuning: delay and bitrate steps are guesses (delay steps from CastMirror).
