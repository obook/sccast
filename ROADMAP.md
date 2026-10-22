# Roadmap

## Screen mirroring (default since v0.3.0)

Google's mirroring receiver (`0F5096E8`), as Chrome does: about 0.15 s of
delay. Notes in [docs/cast-streaming.md](docs/cast-streaming.md).

Done:

- [x] Check that the mirroring receiver accepts a sender that is not Chrome (on the Sharp TV)
- [x] Python prototype (`proto/`); `cast_sender` from openscreen dropped, our own sender was simpler
- [x] Rust sender, `--format mirror`, video and sound: 153 ms on the Sharp
- [x] Resend lost packets at most once per round trip (50 ms or more), cut the bitrate at most once a second
- [x] Read the TV's acknowledgements and measure the round trip
- [x] Menu: Enter, `s`, `q` and closing the terminal free the TV
- [x] Mirror by default, HLS on a device without it
- [x] Reconnect when the TV drops the session or stops answering (TV network cut for 20 s)
- [x] Raise the playout delay from 100 to 400 ms on losses, lower it when calm (the Sharp applies it: 180 ms forced at 100, 500 ms at 400)
- [x] Test on a Nest Hub: takes 1080p in mirror, no adaptive delay. In HLS it plays 720p and refuses 1080p, with a clear message
- [x] Encode at the screen size the TV reports (Nest Hub: 1280x720); the next cast starts from the user's resolution
- [x] Force a key frame every second until the TV acknowledges a video frame
- [x] Change the audio delay during a cast

To do:

- [ ] Test on the school Chromecast
- [ ] Test the HLS fallback on a device without mirroring
- [ ] Test with a firewall on (ufw, firewalld)
- [ ] Tune the bitrate and delay steps on a lossy network
- [ ] Read `rtt_ms` on the school Chromecast (TCP connect took 170 to 500 ms there): delay steps may need to go above 400 ms
- [ ] Lip sync: sound late in mirror on the Sharp (about 250 ms by eye), not in HLS. Measure it, then decide
- [ ] Authenticate the TV (Cast device auth, as chromiacast does)

## Interactive panels

Moved to [sbcast](https://github.com/obook/sbcast) on 2026-10-22.

## HLS

sccast's receiver page and HTTP server: about 3 s of delay.

Done:

- [x] Find Cast devices on the network (mDNS)
- [x] Control the receiver (CASTV2): launch, load, stop, heartbeat
- [x] Capture the screen through the Wayland portal, encode H.264 and AAC, `--audio-delay`
- [x] sccast's own receiver page: 2 to 3 s of delay instead of 5, fallback to Google's receiver
- [x] Load the stream again when the TV refuses it or gives up, reconnect when the session drops
- [x] Warning on the TV, top right: network too slow or frequent buffering
- [x] The receiver page closes itself after 30 s of buffering (sccast gone, PC turned off)
- [x] Continuous Matroska stream (`--format mkv`): 5 to 6 s of delay, kept as an option
- [x] Latency 1.5 s by default: at 1 s the Sharp buffered as soon as the PC was in use

To do:

- [ ] Check the TV warning on a congested network (school)
- [ ] Unpublish the receiver in the Google Cast console and check the fallback to Google's receiver
- [ ] Test the reconnection on a device that drops the session on a slow network
- [ ] Decide whether to keep the "segment sent" measure: it only measures the copy to the kernel

## Menu

Done:

- [x] Menu (ratatui): devices, presets and settings, prefilled from the command line
- [x] Stays during the cast: state, duration, buffering, CPU, bitrate, alerts
- [x] Scrollbar and count for long device lists
- [x] Retry the launch and the reconnection without limit (a first launch on a Chromecast failed after 63 s, the next took 2 s)
- [x] Closing the terminal or the Konsole tab frees the TV

To do:

- [ ] Alert when the TV buffers often (more than 2 per minute)
- [ ] Tune the alert thresholds on the school network
- [ ] Check the launch retry on a cold Chromecast (just plugged in)

## Windows (not started)

Discovery, Cast control, the HTTP server, the menu, the mirror sender and
the encoding already use cross-platform crates and GStreamer. Not checked:
that `gstreamer-rs` builds on Windows.

To do:

- [ ] Screen capture: [`d3d11screencapturesrc`](https://gstreamer.freedesktop.org/documentation/d3d11/d3d11screencapturesrc.html) instead of the portal and `pipewiresrc`
- [ ] PC sound: [`wasapi2src`](https://gstreamer.freedesktop.org/documentation/wasapi2/wasapi2src.html) with `loopback=true` instead of `pulsesrc`
- [ ] AES keys from [`getrandom`](https://docs.rs/getrandom) instead of `/dev/urandom`
- [ ] Menu CPU and load without `/proc`, or leave them out
- [ ] Build with the GStreamer MSVC runtime and development installers ([download](https://gstreamer.freedesktop.org/download/), [gstreamer-rs](https://gitlab.freedesktop.org/gstreamer/gstreamer-rs) for the setup)
- [ ] Installer that ships or requires the GStreamer runtime

Before starting, try [CastMirror](https://github.com/vindeckyy/CastMirror): a Windows app that already does screen mirroring to a Chromecast, with sound. [chromiacast](https://github.com/halfline/chromiacast) is a Rust Cast Streaming sender library, without capture.

## OpenBoard (idea)

OpenBoard has no runtime plugin system, and its W3C widgets cannot start a
program or capture the screen. The light way: a button in the
[OpenBoard fork](https://github.com/obook/OpenBoard) that starts
`sccast cast --device ...` with `QProcess` and stops it with SIGTERM,
which already frees the TV. Linux only until the Windows port.

To do:

- [ ] `sccast list --json`, for another program to offer the device choice
- [ ] Stable status lines on the output (casting, reconnecting, error), for OpenBoard to show
- [ ] "Cast to the TV" button in the OpenBoard fork

## Packaging and code

Done:

- [x] .deb packages for KDE neon and Ubuntu 26.04, releases v0.1.0 to v0.4.1
- [x] Builds with Rust 1.92 or newer
- [x] Sources in folders (capture, device, hls, menu, mirror, session), each file under 200 lines of code
- [x] Install the v0.3.0 .deb on another KDE neon PC and cast

To do:

- [ ] Split `src/mirror/mod.rs`: 309 lines of code since the reconnection
- [ ] Install the v0.4.1 .deb on Ubuntu 26.04 and cast
- [ ] On Ubuntu 26.04 (GNOME), check the screen picker and the capture timestamps
- [ ] Cast for one hour without a cut
- [ ] Stop the screen sharing from the desktop during a cast: the TV must be freed and the error shown

If needed:

- [ ] Detect a missing `x264enc` at start and suggest `openh264enc` or VA-API
- [ ] Hardware encoder (`--hw`)
- [ ] X11 capture (`ximagesrc`)
