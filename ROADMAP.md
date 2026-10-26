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
- [x] Change the audio delay during a cast (restarts it: a live change froze the image)
- [x] Negative audio delay: the image waits (-300 ms on the Sharp in mirror)
- [x] Positive audio delay: the sound waits for its date before it leaves (the TV played it on arrival)
- [x] Convert the frames on every core: an i5-7600 lost about 7 frames a second at 1080p60
- [x] Convert only the frames the screen sends: during a video, 24 to 32 real frames a second reach the Nest Hub instead of 13 to 25
- [x] Test on a 77" OLED TV (S85F, Cast built in): mirror works, the TV reports a 3840x2160 screen at 60 fps, relaunches take 2.5 to 5 s
- [x] The menu's Format line shows HLS during the HLS fallback (it showed Mirror); the next cast tries mirror again

To do:

- [ ] Test on the school Chromecast
- [ ] Test the HLS fallback on a device without mirroring, and the Format line shown then
- [ ] S85F: the first launch of the evening was refused (`CANCELLED` after 7 s) and the cast fell back to HLS for 3 min, while the next launches worked. Test a cold launch (TV on another source for a minute); if it is refused again, retry the mirroring launch once before falling back to HLS
- [ ] CPU on an i7-9750H during a YouTube video at 1080p: about 300 % of a core in mirror (up to 545 %), 200 % in HLS (up to 466 %), and the capture fell to 41-50 frames a second. Profile the frame conversion with perf
- [ ] Test with a firewall on (ufw, firewalld)
- [ ] Tune the bitrate and delay steps on a lossy network
- [ ] Read `rtt_ms` on the school Chromecast (TCP connect took 170 to 500 ms there): delay steps may need to go above 400 ms
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
- [x] Remove the "segment sent" measure: it timed the copy to the kernel, not the TV download

To do:

- [ ] Check the TV warning on a congested network (school)
- [ ] Unpublish the receiver in the Google Cast console and check the fallback to Google's receiver
- [ ] Test the reconnection on a device that drops the session on a slow network

## Menu

Done:

- [x] Menu (ratatui): devices, presets and settings, prefilled from the command line
- [x] Stays during the cast: state, duration, buffering, CPU, bitrate, alerts
- [x] Scrollbar and count for long device lists
- [x] Retry the launch and the reconnection without limit (a first launch on a Chromecast failed after 63 s, the next took 2 s)
- [x] Closing the terminal or the Konsole tab frees the TV
- [x] Alert when the TV buffers more than twice a minute
- [x] Audio delay in 50 ms steps, from -500 to 1500 ms

To do:

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

Done:

- [x] `sccast list --json`, for another program to offer the device choice
- [x] State lines on stdout (casting, buffering, reconnecting, stopped, error), messages on stderr

To do:

- [ ] "Cast to the TV" button in the OpenBoard fork

## Packaging and code

Done:

- [x] .deb packages for KDE neon and Ubuntu 26.04, releases v0.1.0 to v0.4.5
- [x] GitHub builds the .deb packages and attaches them when a release is published
- [x] Builds with Rust 1.92 or newer
- [x] Sources in folders (capture, device, hls, menu, mirror, session), each file under 200 lines of code
- [x] Install the v0.3.0 .deb on another KDE neon PC and cast
- [x] Name a missing GStreamer element and its package at start (a build from source may lack `x264enc`)

To do:

- [ ] Install the v0.4.1 .deb on Ubuntu 26.04 and cast
- [ ] On Ubuntu 26.04 (GNOME), check the screen picker and the capture timestamps
- [ ] Cast for one hour without a cut
- [ ] Stop the screen sharing from the desktop during a cast: the TV must be freed and the error shown

If needed:

- [ ] Fall back to `openh264enc` or VA-API without `x264enc`
- [ ] Hardware encoder (`--hw`)
- [ ] 4K, set aside on 2026-10-26: the S85F accepts 3840x2160, but the PC screens are 1080p (the TV upscales them itself) and x264 would need about 4 times the CPU of 1080p (estimate). Needs a 4K screen and the hardware encoder; the menu stops at 1920x1080 and 10 Mbit/s
- [ ] X11 capture (`ximagesrc`)
