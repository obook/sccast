# Roadmap

## Stage 1: HLS

Done:

- [x] Find Cast devices on the network (mDNS)
- [x] Control the receiver (CASTV2): launch, load, stop, heartbeat
- [x] Serve a local file or the HLS stream over HTTP
- [x] Capture the screen through the Wayland portal and encode it as H.264 HLS
- [x] Send the PC sound (AAC), with `--audio-delay`
- [x] Stream options, device picker, terminal messages and spinner
- [x] Load the stream again when the receiver gives up
- [x] Relaunch the receiver when the device drops the session
- [x] sccast's own receiver page: 2 to 3 s of delay instead of 5
- [x] Fall back to Google's Default Media Receiver when sccast's does not start
- [x] Build script for the .deb packages (KDE neon, Ubuntu 26.04)

To do:

- [ ] Cast for one hour without a cut
- [ ] Test the reconnection on a device that drops the session on a slow network
- [ ] Stop the screen sharing from the desktop during a cast: the TV must be freed and the error shown
- [ ] Install each .deb on a clean system of its target and cast
- [ ] On Ubuntu 26.04 (GNOME), check the screen picker and the capture timestamps
- [ ] First GitHub release with both .deb
- [x] Launch menu (ratatui): device list, presets and settings, prefilled from the command line
- [ ] Keep the menu during the cast: state, change settings or device, stop, quit with Ctrl+C
- [ ] Warn when the PC is too loaded to stream (dropped frames, late segments)

If needed:

- [ ] Detect a missing `x264enc` at start and suggest `openh264enc` or VA-API
- [ ] Hardware encoder (`--hw`)
- [ ] X11 capture (`ximagesrc`)

## Lower latency on HLS (on hold)

2 to 3 s is enough for now. Ideas kept for later:

- [ ] Segments shorter than 1 s (`hlssink2` cannot: its target duration is a whole number of seconds)
- [x] Continuous Matroska stream (`--format mkv`): plays, but 5 to 6 s of delay, the TV's native player keeps its own buffer
- [ ] Progressive fragmented MP4 instead of HLS (likely the same buffering as Matroska, not checked)
- [ ] WebRTC in the receiver page (not checked)

## Stage 2: Cast Streaming

Real-time mirroring, as Chrome does, aiming at a fraction of a second.

- [ ] Build `cast_sender` from [openscreen](https://chromium.googlesource.com/openscreen/) and cast a video file to a Chromecast
- [ ] Check whether `cast_sender` can take a live stream, so sccast could run it instead of a new sender
- [ ] Otherwise write the sender in Rust: OFFER/ANSWER, RTP encrypted with AES-CTR, RTCP, bitrate adaptation
