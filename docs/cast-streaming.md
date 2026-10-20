# Stage 2: Cast Streaming, first exploration

Date: 2026-10-20. Device: "Sharp Olivier Youyou" (AQUOS Android TV, 192.168.33.62).
No other device was tested.

## Result

A sender written from scratch, outside Chrome, mirrors live H.264 video to the
Sharp through the Chrome Mirroring receiver. On a photo of the PC preview and
the TV side by side, the TV was 133 ms behind, on two photos taken 73 s apart
(PC 22:16:48.199 / TV 22:16:48.066, PC 22:18:01.299 / TV 22:18:01.166; 720p30
test pattern, target playout delay 100 ms). The true
glass-to-glass delay is that plus the preview's own delay on the PC, not
measured (probably a few tens of ms). The HLS path of stage 1 gives 2 to 3 s on
the same TV.

Recommendation: write the stage 2 sender in Rust inside sccast, porting
`proto/stream_proto.py`. Do not build on openscreen.

## What was tried

### 1. Does the mirroring receiver accept a sender that is not Chrome?

Yes, on the Sharp. `proto/offer_probe.py` (standard library only) opens CASTV2,
sends `LAUNCH 0F5096E8`, then an OFFER on `urn:x-cast:com.google.cast.webrtc`.

- The app starts as "Chrome Mirroring" (`appType: WEB`), with the namespaces
  `webrtc`, `media`, `debug` and `remoting`.
- The ANSWER arrives 0.3 to 0.6 s after the OFFER. For an OFFER listing H.264,
  VP8 and Opus, the Sharp picked H.264 and Opus (`sendIndexes: [2, 0]`).
- Nothing authenticates the sender. In openscreen, the trust store only checks
  the receiver's certificate chain (`cast/standalone_sender/main.cc`).

Not verified: the same on a Chromecast dongle or a Nest Hub.

### 2. openscreen `cast_sender`

Not built. Reasons:

- `cast_sender` needs FFmpeg, libopus and libvpx even for H.264
  (`have_external_libs` in `cast/standalone_sender/BUILD.gn`). `libvpx-dev` is
  not installed and installing it needs root.
- The build pulls a Chromium toolchain (clang, Rust, sysroot) through `gclient`,
  several GB (estimate, not measured). The disk has 16 GB free (99 % used).
- It reads a file through `avformat_open_input`, decodes it and encodes it
  again (`simulated_capturer.cc`). Feeding it a live stream through an FFmpeg
  URL or a pipe may work (hypothesis, not tried), but the decode and re-encode
  adds delay and CPU, and its pacing assumes a file.

Once the Python sender worked, building it would only have confirmed what was
already measured. The sources (shallow clone in `~/src/openscreen`) served as
the protocol reference.

### 3. Minimal sender: `proto/stream_proto.py`

About 270 lines of Python, using GStreamer through PyGObject and the
`cryptography` package, both already installed.

- Video: `videotestsrc` with the wall clock (ms) burnt in, then `x264enc
  tune=zerolatency speed-preset=ultrafast bframes=0 key-int-max=60`, then
  `h264parse config-interval=-1` to Annex B access units, then `appsink`.
- Each frame: AES-128-CTR, nonce = `frame_id` (big endian, bytes 8 to 11) XOR
  `aesIvMask` (`frame_crypto.cc`). Split into packets of at most 1400 bytes:
  12 bytes RTP header, 7 bytes Cast header (key frame flag, 8 bit frame id,
  packet id, last packet id, reference frame id), payload (`rtp_packetizer.cc`).
- RTCP Sender Report before the first frame and every 100 ms: NTP from the
  wall clock, RTP timestamp at that instant (90 kHz). The receiver drops
  packets that arrive before the first report (`receiver_impl.cc`).
- RTP and RTCP share one UDP socket, sent to the `udpPort` of the ANSWER.
- Receiver feedback (RTCP payload specific, `CAST` and PLI): NACKed packets are
  resent from a cache of the last 120 frames; a PLI forces a key frame.
- `--preview` shows the same frames on the PC for the photo measurement.

Run: `python3 proto/stream_proto.py 192.168.33.62 --seconds 60 --delay 100 --preview`

Measured on the Sharp over a 3 minute run (720p30, about 3.5 Mbit/s; whether
the TV is on Wi-Fi or Ethernet was not checked):

| Measure | Value |
|---|---|
| Capture to send (encode, encrypt, packetize) | 9 ms typical, 14 ms worst 30 s average |
| Frames acknowledged by the receiver | 5392 of 5393, checkpoint one frame behind the last sent |
| Packets resent after a NACK | 33 in 3 min, session kept going |
| PLI (key frame requests) | none |
| TV behind PC preview (two photos) | 133 ms both times |

Pitfall: `x264enc` shifts PTS by 1000 hours. Without converting through the
segment to running time, frames are stamped in the future; the receiver never
plays them.

Two sessions received `CLOSE` from the receiver early on. The user was casting
to the TV at the same time; that is the likely cause (hypothesis).

## Not done

- Audio (Opus, second stream with its own SSRC, key and Sender Reports).
- Screen capture: the prototype uses a test pattern. sccast already gets
  PipeWire through the portal; the encoder branch can feed an `appsink`.
- Bitrate adaptation. The receiver sends Receiver Reports (loss, jitter) and
  the `CST2` ACK vector; nothing reads them yet.
- PLI path: written, never triggered during the tests.
- Receiver event log (`receiverRtcpEventLog`), which would give the playout
  delay per frame as seen by the receiver.
- Lower target delay (50 ms) and 1080p.

## Rust port, estimate

| Part | Estimate (lines) |
|---|---|
| CASTV2: LAUNCH, OFFER/ANSWER on the `webrtc` namespace (rust_cast; whether 0.21 can send on a custom namespace is not verified) | 100 |
| Encoder branch to `appsink`, PTS to running time | 60 |
| Packetizer, AES-CTR (`aes`, `ctr` crates), Sender Report | 120 |
| RTCP parser, NACK resend, PLI | 120 |
| Opus audio stream | 100 |
| Simple bitrate adaptation from loss | 100 |

Total about 600 lines (estimate), most of it a direct port of the prototype.
The risk is device coverage, not the protocol: the Chromecast dongle and the
Nest Hubs still need testing.
