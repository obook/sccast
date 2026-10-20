# Roadmap

## Screen mirroring (default since v0.3.0)

Google's mirroring receiver (`0F5096E8`), as Chrome does: about 0.15 s of
delay. Notes in [docs/cast-streaming.md](docs/cast-streaming.md).

Done:

- [x] Check that the mirroring receiver accepts a sender that is not Chrome (on the Sharp TV)
- [x] Python prototype (`proto/`); `cast_sender` from openscreen dropped, our own sender was simpler
- [x] Rust sender, `--format mirror`, video and sound: 153 ms on the Sharp
- [x] Resend lost packets at most every 50 ms, cut the bitrate at most once a second
- [x] Menu: Enter, `s`, `q` and closing the terminal free the TV
- [x] Mirror by default, HLS on a device without it

To do:

- [ ] Test on the school Chromecast and on a Nest Hub (one accepted a 5 s session)
- [ ] Test the HLS fallback on a device without mirroring
- [ ] Test with a firewall on (ufw, firewalld)
- [ ] Reconnect when the TV drops the session
- [ ] Tune the bitrate steps and the target delay (100 ms) on a lossy network
- [ ] Measure the lip sync

## HLS

sccast's receiver page and HTTP server: 2 to 3 s of delay.

Done:

- [x] Find Cast devices on the network (mDNS)
- [x] Control the receiver (CASTV2): launch, load, stop, heartbeat
- [x] Capture the screen through the Wayland portal, encode H.264 and AAC, `--audio-delay`
- [x] sccast's own receiver page: 2 to 3 s of delay instead of 5, fallback to Google's receiver
- [x] Load the stream again when the TV refuses it or gives up, reconnect when the session drops
- [x] Warning on the TV, top right: network too slow or frequent buffering
- [x] The receiver page closes itself after 30 s of buffering (sccast gone, PC turned off)
- [x] Continuous Matroska stream (`--format mkv`): 5 to 6 s of delay, kept as an option

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

## Packaging and code

Done:

- [x] .deb packages for KDE neon and Ubuntu 26.04, releases v0.1.0, v0.2.0 and v0.3.0
- [x] Builds with Rust 1.92 or newer
- [x] Sources in folders (capture, device, hls, menu, mirror, session), each file under 200 lines of code

To do:

- [ ] Install the v0.3.0 .deb on a clean system of each target and cast
- [ ] On Ubuntu 26.04 (GNOME), check the screen picker and the capture timestamps
- [ ] Cast for one hour without a cut
- [ ] Stop the screen sharing from the desktop during a cast: the TV must be freed and the error shown

If needed:

- [ ] Detect a missing `x264enc` at start and suggest `openh264enc` or VA-API
- [ ] Hardware encoder (`--hw`)
- [ ] X11 capture (`ximagesrc`)
