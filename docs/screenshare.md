# Interactive panels (`--format screenshare`)

Goal: cast to the receiver of Bytello Share panels, as their Android
sender does in its "classic" mode. No panel has been tested yet: everything
below is read from the decompiled Android sender (version from October 2026)
and is unverified on a real panel.

File references point into the jadx output of the APK
(`sources/`, short names given by jadx).

## Finding a panel

- mDNS, service `_sspcast._tcp`. sccast browses it with the Chromecasts:
  panels show in `sccast list` and in the menu as "Screenshare", and
  choosing one casts in this format. The sender reads these TXT keys
  (`com/seewo/swstclient/discover/e.java`): `id`, `name`, `host`, `port`,
  `mac`, `pv`, `dv`, `useHost`. sccast takes `host` and `port` from the TXT
  record when present, else the A and SRV records. Hypothesis: `pv` and `dv`
  are the protocol and device versions; sccast does not use them.
- SSDP: M-SEARCH with `ST: _sspcast._tcp` to 239.255.255.250 and
  239.255.255.150, port 1990 (`com/seewo/libdiscovery/ssdp/a.java`).
  Not built: only worth it if a real panel does not answer mDNS.
- The 6-character code shown on the panel holds its address. Decoding read
  from the Windows client (`PinCodeService.dll`, `ParsePinCode`) and the
  Android port table (`com/bytello/libpincode/c.java`), in
  `src/screenshare/pin.rs`: W, X, Y, Z stand for 0, 1, I, O; six digits give
  192.168.x.x; a base-32 value gives 10.x.x.x or 172.16.0.0/12, masked with
  a small key; 2 bits pick the command port among 7385, 29736, 2067 and
  39458. Other codes need the vendor's cloud lookup: not supported.
  Unverified on a real panel.
- Without discovery: `sccast cast --format screenshare --pin CODE`, or
  `--host IP:PORT`. In the menu, `c` casts to a panel by its code.

No TLS and no encryption seen on the command, video or audio sockets
(unverified on the wire).

## Command frames

Each frame is a protobuf varint32 length, then a `BytelloMessage`
(`bytello_channel.proto`, `com/seewo/easiair/protocol/codec/CommandEncoder.java`):

| Field | Number | Type |
|---|---|---|
| version | 1 | int32, 1 from the sender |
| commandType | 2 | int32 |
| commandId | 3 | int32 |
| sequence | 4 | int32 |
| timestamp | 5 | int64, ms since 1970 |
| payload | 6 | string, JSON |

The payload is the Gson form of the message object, without the five fields
above. Field names are the Java names (no renaming annotation seen).
Gson leaves out null fields.

An older receiver uses another framing, built by a native library
(`SWSTProtocolSdk.frameToBuffer`, not in the APK). The sender detects it when
the receiver's first message has version 0 or fails to parse
(`CommandDecoder.java`). sccast does not support it.

Ids (`com/seewo/easiair/protocol/ProtocolConstant.java`):

| Type | Id | Message |
|---|---|---|
| 0 | 0 | heartbeat `{sentTimeMs, receivedTimeMs}` |
| 1 | 1 | connect request |
| 1 | 101 | connect response |
| 1 | 102 | receiver disconnects |
| 6 | 2 | cast request |
| 6 | 102 | cast response |
| 6 | 3 | stop cast (sender) |
| 6 | 103 | stop cast (receiver) |
| 6 | 105 | receiver wants another size or frame rate |
| 6 | 113 | receiver wants a key frame |

## Session

1. Connect request (`com/seewo/swstclient/logic/a.java`): `deviceName`,
   `deviceType` (1 on Android), `pinCode`, `connectType` 0,
   `deviceVersion`, `deviceUuid`, `functions`, `originIp`.
2. Connect response: `resultType` 1 is success; -100 connection error;
   -101 too many senders. Its `version` is the receiver's protocol version.
3. Cast request (`com/seewo/swstclient/module/screen/logic/t.java`):
   `authorizeMode`, `supportAudio`, `displayMode` 0,
   `audio {codec 11, sampleRate 44100, channel 2, adtsEnabled}`,
   `video {width, height}`. Hypothesis: codec 11 is AAC, since the encoder is
   AAC LC (`com/seewo/libscreencamera/base/d.java`).
4. Cast response: `resultType` 1, then `videoPort`, `audioPort`,
   `ctcpPort`, `bitrate`.
5. The sender opens one TCP connection to `videoPort` and one to `audioPort`
   on the same host, with TCP_NODELAY.

A person may have to allow the sender on the panel (a dialog, sometimes a
code shown on the panel). In the menu, Enter on a panel asks for that code;
sccast sends it as `pinCode` (hypothesis: the field takes the code as
typed). sccast waits up to 90 s for each answer, with a countdown. A refusal
or no answer ends the cast without reconnecting. Answer codes, from the
Android sender's action names: -100 failure, -101 too many devices, -102
refused, -103 panel not ready; from the Windows client's error table
(`bsprotocol.dll`): -7 session not found, -8 refused by the user, -9 a
session with this id already exists, -10 taken by another sender, -11 too
many casts on the panel, -12 session disconnected, -13 too many casts from
this sender. On -9 and -12 sccast tries again every 5 s for 35 s: the panel
drops a lost session only after 30 s without news (Windows client's
timeout, hypothesis for the panel).

Connect request as the Windows client builds it: `pincode` and `pinCode`
(both spellings appear; sccast sends the code typed), `deviceName`,
`platform` (values not decoded, not sent), `deviceType` (the Windows client
writes 5; sccast too, hypothesis: a PC), `connectType`, `deviceVersion`,
`deviceUuid` (the same across reconnections), `originIp`,
`supportMultiTouch`, `supportPen`. Answer keys: `resultType`, `failReason`,
`receiverType`, `receiverName`, `maxWindowCount`.

Test on site. `sccast pin CODE` prints the address a code gives, without
connecting, to compare with the panel's network settings. Each panel cast
writes a protocol trace to `~/.local/state/sccast/screenshare.log`, kept
after logout and started over at each run: connection attempts (sccast
tries the 4 command ports when the first does not answer), every command
sent and received with its JSON (cut at 500 characters), the first 3 video
frames, and each loss with its cause.

Losses and delay. At school the vendor's own app often loses the panels,
so sccast plans for it:
- After a loss (EOF, RST, a write error, nothing read for 30 s, or 10 s
  without sending a byte), sccast connects again to the same address with
  the same device id, asks for the cast again and restarts the video on a
  key frame. Up to 3 times in 5 min from the terminal, without limit from
  the menu. Each cause goes to the log (`$XDG_RUNTIME_DIR/sccast.log` from
  the menu) with its time. Hypothesis: a known device id may spare a new
  approval on the panel.
- TCP queues what the network cannot carry. The media sockets have a
  64 KiB send buffer; a frame that comes while the previous one is still
  unsent is dropped, the video then waits for a key frame (asked to the
  encoder), and the bitrate goes down by a fifth, back up by a tenth after
  10 s without drops (as mirror). The menu's Live panel counts the drops.

Heartbeat (`com/seewo/swstclient/module/network/codec/b.java`): after 3 s
without writing, the sender sends one. After 10 s without reading it sends
one too. After two such silences it gives up.

## Video

H.264 in Annex B (start codes), from Android's MediaCodec. Each frame on the
video socket (`codec/ScreenProjectionEncoder.java`, `network/w.java`):

    int32 length | int32 index | int32 type | data

All integers are big endian. `index` counts frames from 0 on this
connection. It is only there when the receiver's version is 63 or more
(`module/base/util/K.java`). `type` is always 0 for video.

At the start the sender sends SPS and PPS as two separate frames
(`network/util/d.java`), then one frame per encoded buffer. sccast does the
same: SPS and PPS out of each key frame, then the rest.

## Audio

AAC LC, 44.1 kHz, stereo, each frame with a 7-byte ADTS header. Frames on
the audio socket (`network/a.java`) have no `index`:

    int32 length | int32 type | data

Here `type` carries the sender's monotonic clock in ms, cut to 32 bits
(`screen/helper/c.java`). Unverified how the receiver uses it.

## Touchback

Touches on the panel drive the PC. Read from the Windows client
(`SWMediaTouchBackService.dll`, decompiled with Ghidra) and the Android
message table; unverified on a real panel.

1. The panel asks on the command channel: id 1 to start, id 2 to quit
   (`SWMediaTouchBackService::TryParse`). The command type is set by the
   caller, outside this DLL: unknown. Android's table has (6, 6) with
   `toggle` for the same thing. sccast takes both: (6, 6), or id 1 or 2 with
   a command type it does not know.
2. The PC opens a UDP socket on any port and answers with the same type,
   id + 100, the request's sequence, and `resultType` 1, `failReason`,
   `udpServerPort` (keys from Android's `TouchbackPortResponse`; the Windows
   message holds the same three values).
3. The panel sends each touch to that port: one protobuf message per
   datagram, no length, command type 3, payload `UdpPoint`
   (`enable`, `mPort`, `pointX`, `pointY`, `x`, `y`). Ids: 21 and 22 left
   press and release, 23 and 24 right, 25 move, 26 left click, 27 right
   click, 28 double click, 29 wheel.

On Windows the touches reach `RemoteControlClient::MouseMove(bool, int x,
int y)` and friends (`SWSTRemoteControlTool.dll`), then
`RemoteControlServer::InjectPrivilegedMouse`, which calls `SendInput`.
With the flag false, x and y are absolute pixels of the PC's desktop,
checked against its bounds (`GetSystemMetrics` 76 to 79). With the flag
true, they are a relative move. Hypothesis: the flag is `enable`, and the
panel scales touches to the video size sent in the cast request.
Unknown: which pair (`x, y` or `pointX, pointY`) carries them. The Windows client also sends a UDP beat (type 3, id 81)
to the panel; where to is unclear, so sccast does not send it.

In sccast: the Touchback setting (menu row, or `--touchback`) opens a
second portal session after the screen dialog: a RemoteDesktop session,
for which KDE asks to allow remote control. It is not used for the
capture: KDE's RemoteDesktop dialog lets the user pick no screen, and with
three monitors the whole workspace was cast once. Absolute pointer motion
needs a stream of that session, so sccast takes the one that contains the
shared monitor (one per monitor, or the whole workspace, both handled) and
adds the monitor's offset in it. A shared window turns touchback off. The
touches then move the pointer to the matching point of the shared screen,
black bands of the video left out, and press the buttons; the wheel moves one notch per event, its direction taken from
the sign of `y` (hypothesis). With the setting off, sccast refuses the
panel's request (`resultType` -100), so the panel can say so.

## Not done

- CTCP: a third connection for congestion control and network time
  (`com/cvte/libcongestioncontroller`). The sender opens it only in one mode
  of its settings. sccast skips it.
- Bitrate changes from the cast response, and size changes (id 105).
- The PIN decoding, the cloud lookup and the WebRTC mode.
