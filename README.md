# sccast

`sccast` casts a Linux desktop's screen and sound to a Chromecast or any
device with Chromecast built-in, as HLS served over HTTP on your local
network and played by Google's Default Media Receiver.

It does not use Google's screen mirroring, the one Chrome and Brave use.
Expect about 5 seconds of delay on the TV.

## Requirements

- A Wayland session with `xdg-desktop-portal` (tested on KDE Plasma).
- PipeWire, with `pipewire-pulse` for sound.
- GStreamer (tested with 1.24).
- A Rust toolchain (`cargo`).
- PC and TV on the same local network, mDNS (UDP 5353) allowed between
  them.

## Install

Debian, Ubuntu and derivatives (tested on KDE neon 24.04):

```sh
sudo apt install pkg-config libgstreamer1.0-dev gstreamer1.0-tools \
  gstreamer1.0-plugins-{base,good,bad,ugly} gstreamer1.0-libav \
  gstreamer1.0-pipewire
```

Fedora (not tested; `x264enc` comes from RPM Fusion):

```sh
sudo dnf install pkgconf gstreamer1-devel gstreamer1-plugins-{base,good,bad-free} \
  gstreamer1-plugins-ugly gstreamer1-plugin-libav pipewire-gstreamer
```

Arch Linux (not tested):

```sh
sudo pacman -S pkgconf gstreamer gst-plugins-{base,good,bad,ugly} gst-libav \
  gst-plugin-pipewire
```

Check the GStreamer elements, then build:

```sh
gst-inspect-1.0 pipewiresrc pulsesrc x264enc hlssink2 avenc_aac > /dev/null
cargo build --release
```

The binary is `target/release/sccast`.

## Firewall

Open TCP port 8090 to your local network:

```sh
sudo ufw allow from 192.168.1.0/24 to any port 8090 proto tcp   # ufw
sudo firewall-cmd --add-port=8090/tcp --permanent && sudo firewall-cmd --reload   # firewalld
```

Replace `192.168.1.0/24` with your network.

## Usage

```sh
sccast list                                  # devices on the network
sccast cast                                  # pick a device, then a screen
sccast cast --device "Living Room TV"        # name or id from `sccast list`
sccast cast --device "Kitchen" --resolution 1280x720
sccast cast --device "Kitchen" --file movie.mp4
sccast stop --device "Kitchen"               # close a cast left running
sccast serve                                 # local stream, no TV
```

`cast` opens the screen picker, waits about 6 seconds for the stream to
fill, then starts the TV. Ctrl+C stops the TV at once.

`serve` prints a URL for `ffplay`, to tell a capture problem from a TV
problem. VLC stutters on this live stream and is not a reliable check.
`serve --test-pattern` streams a test image and ticks instead.

| Option | Default | Effect |
|---|---|---|
| `--resolution WxH` | `1920x1080` | Output size, letterboxed to keep the screen shape |
| `--fps N` | `30` | Frame rate |
| `--bitrate KBPS` | `4000` | Video bitrate in kbit/s |
| `--port N` | `8090` | HTTP port the TV fetches the stream from |
| `--no-audio` | | Video only |
| `--audio-delay MS` | `0` | Delays the sound on the TV (`cast` only) |
| `--url URL` | | Plays a media URL instead of the screen (`cast` only) |
| `--file PATH` | | Plays a local MP4 file instead of the screen (`cast` only) |

## Custom receiver (experimental)

`docs/receiver/` is a receiver page that plays closer to live than the
Default Media Receiver and shows the delay in a corner of the TV. It is
served at <https://obook.github.io/sccast/receiver/>.

1. Register at the [Google Cast SDK Developer Console](https://cast.google.com/publish)
   ($5, once).
2. Add an application of type Custom Receiver with the page URL above.
   Note its application ID.
3. Add your TV under Devices with its serial number. Wait 15 minutes, then
   unplug the TV or Chromecast and plug it back in.
4. `sccast cast --receiver <application ID>`

An unpublished application only runs on the devices registered in step 3.

## Troubleshooting

No device found: the PC may block mDNS (UDP 5353).

Stuck on the Cast logo: the firewall most likely blocks port 8090.
`RUST_LOG=sccast=info,tower_http=debug` shows whether the TV requests the
stream.

"refused the stream": the device cannot decode the format. A Google Nest
Hub plays `--resolution 1280x720` but refuses 1080p.

Sound before image: the PC speakers play live, 5 seconds ahead of the TV.
Turn them down with their hardware knob; muting the PC in the desktop also
mutes the stream. If the TV sound leads its image, add `--audio-delay 200`
and adjust.

The TV drops back to its home screen: `sccast` launches the receiver again,
up to 3 times in 5 minutes. If the device no longer shows in `sccast list`,
unplug it and plug it back in.

Logs: `RUST_LOG=sccast=debug sccast cast`.

## Limitations

- About 5 seconds of latency: the TV buffers a few HLS segments before it
  plays them.
- The Default Media Receiver draws a "LIVE" badge.

## Author

Olivier Booklage <olivier.booklage@ac-bordeaux.fr>

## License

MIT, see [LICENSE](LICENSE).

The Rust dependencies use MIT, Apache-2.0, ISC or BSD licenses. At run
time, `sccast` loads GStreamer (LGPL-2.1 or later) and its `x264enc`
plugin, which wraps x264 (GPL-2.0 or later).
