#!/usr/bin/env python3
"""Minimal Cast Streaming sender: H.264 video only, to the Chrome Mirroring receiver.

Usage: stream_proto.py DEVICE_IP [--delay MS] [--seconds N] [--src GST_SOURCE]
The default source is a test pattern with the sender's wall clock (ms) burnt in,
to measure the glass-to-glass delay against a clock shown on the PC.
Wire format follows openscreen/cast/streaming/impl (rtp_packetizer, frame_crypto,
sender_report_builder, compound_rtcp_parser).
"""
import argparse
import json
import os
import socket
import struct
import threading
import time
from datetime import datetime

import gi
gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes  # noqa: E402

from offer_probe import Channel, MIRRORING_APP, NS_CONN, NS_RECEIVER, NS_WEBRTC  # noqa: E402

SSRC = 0x5CCA57
PAYLOAD_TYPE = 96  # 127 is what Chrome uses for video; the receiver accepted 96 in the probe.
MAX_PACKET = 1400
HEADER = 19  # 12 bytes RTP + 7 bytes Cast header
NTP_EPOCH_OFFSET = 2208988800
SR_INTERVAL = 0.1


def ntp(t):
    sec = int(t) + NTP_EPOCH_OFFSET
    return (sec << 32) | int((t % 1) * (1 << 32))


class Sender:
    def __init__(self, host, port, key, iv_mask):
        self.dest = (host, port)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("0.0.0.0", 0))
        self.key, self.iv_mask = key, iv_mask
        self.seq = int.from_bytes(os.urandom(2), "big")
        self.frame_id = -1
        self.cache = {}  # frame id -> list of packets, for retransmission
        self.lock = threading.Lock()
        self.packets = self.octets = 0
        self.stats = {"nack_pkts": 0, "pli": 0, "ckpt": None, "delay": None, "rr": 0}
        self.wall0 = None  # wall clock at pipeline running time 0
        self.rtp_base = int.from_bytes(os.urandom(4), "big")

    def rtp_at(self, t):
        return (self.rtp_base + int((t - self.wall0) * 90000)) & 0xFFFFFFFF

    def encrypt(self, frame_id, data):
        nonce = bytearray(16)
        struct.pack_into(">I", nonce, 8, frame_id & 0xFFFFFFFF)
        nonce = bytes(a ^ b for a, b in zip(nonce, self.iv_mask))
        enc = Cipher(algorithms.AES(self.key), modes.CTR(nonce)).encryptor()
        return enc.update(data) + enc.finalize()

    def send_frame(self, data, capture_wall, key):
        with self.lock:
            self.frame_id += 1
            fid = self.frame_id
            payload = self.encrypt(fid, data)
            chunk = MAX_PACKET - HEADER
            n = max(1, -(-len(payload) // chunk))
            ref = fid if key else fid - 1
            ts = self.rtp_at(capture_wall)
            pkts = []
            for pid in range(n):
                head = struct.pack(">BBHII", 0x80, (0x80 if pid == n - 1 else 0) | PAYLOAD_TYPE,
                                   self.seq & 0xFFFF, ts, SSRC)
                self.seq += 1
                cast = struct.pack(">BBHHB", (0x80 if key else 0) | 0x40, fid & 0xFF, pid, n - 1, ref & 0xFF)
                pkts.append(head + cast + payload[pid * chunk:(pid + 1) * chunk])
            self.cache[fid] = pkts
            self.cache.pop(fid - 120, None)
            for p in pkts:
                self.sock.sendto(p, self.dest)
                self.packets += 1
                self.octets += len(p) - HEADER

    def send_sr(self):
        now = time.time()
        with self.lock:
            body = struct.pack(">IQIII", SSRC, ntp(now), self.rtp_at(now), self.packets, self.octets)
        self.sock.sendto(struct.pack(">BBH", 0x80, 200, len(body) // 4) + body, self.dest)

    def expand(self, low8):
        fid = (self.frame_id & ~0xFF) | low8
        return fid - 256 if fid > self.frame_id else fid

    def on_rtcp(self, buf, request_key_frame):
        i = 0
        while i + 4 <= len(buf):
            b0, pt, length = struct.unpack_from(">BBH", buf, i)
            body = buf[i + 4:i + 4 + length * 4]
            i += 4 + length * 4
            fmt = b0 & 0x1F
            if pt == 201:
                self.stats["rr"] += 1
            elif pt == 206 and fmt == 1:
                self.stats["pli"] += 1
                request_key_frame()
            elif pt == 206 and fmt == 15 and body[8:12] == b"CAST":
                ckpt, nloss, delay = struct.unpack_from(">BBH", body, 12)
                with self.lock:
                    self.stats["ckpt"] = self.expand(ckpt)
                    self.stats["delay"] = delay
                    for k in range(nloss):
                        f8, pid, bits = struct.unpack_from(">BHB", body, 16 + 4 * k)
                        self.resend(self.expand(f8), pid, bits)

    def resend(self, fid, pid, bits):
        pkts = self.cache.get(fid)
        if not pkts:
            return
        if pid == 0xFFFF:
            wanted = range(len(pkts))
        else:
            wanted = [pid] + [pid + 1 + b for b in range(8) if bits >> b & 1]
        for p in wanted:
            if p < len(pkts):
                self.sock.sendto(pkts[p], self.dest)
                self.stats["nack_pkts"] += 1


def control_loop(ch, stop):
    while not stop.is_set():
        try:
            r = ch.recv(1)
        except (OSError, ConnectionError) as e:
            print("control channel closed:", e)
            stop.set()
            return
        if r:
            print(f"<- {time.strftime('%H:%M:%S')} {r[1]} {r[0]} {json.dumps(r[2])[:700]}")


def launch(ch, offer):
    ch.send("receiver-0", NS_CONN, {"type": "CONNECT"})
    ch.send("receiver-0", NS_RECEIVER, {"type": "LAUNCH", "appId": MIRRORING_APP, "requestId": 0})
    deadline = time.time() + 30
    transport = None
    while time.time() < deadline and not transport:
        r = ch.recv(5)
        status = r[2].get("status") if r else None
        for a in status.get("applications", []) if isinstance(status, dict) else []:
            if a.get("appId") == MIRRORING_APP and "transportId" in a:
                transport = a["transportId"]
    if not transport:
        raise SystemExit("mirroring app did not start")
    ch.send(transport, NS_CONN, {"type": "CONNECT"})
    ch.send(transport, NS_WEBRTC, offer)
    while time.time() < deadline:
        r = ch.recv(5)
        if r and r[2].get("type") == "ANSWER":
            return r[2]
    raise SystemExit("no ANSWER")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("--delay", type=int, default=100, help="target playout delay, ms")
    ap.add_argument("--seconds", type=int, default=60)
    ap.add_argument("--bitrate", type=int, default=4000, help="kbit/s")
    ap.add_argument("--size", default="1280x720")
    ap.add_argument("--src", default=None, help="GStreamer source producing raw video")
    ap.add_argument("--preview", action="store_true", help="also show the frames on the PC")
    args = ap.parse_args()
    w, h = map(int, args.size.split("x"))

    key, iv_mask = os.urandom(16), os.urandom(16)
    offer = {"type": "OFFER", "seqNum": 1, "offer": {"castMode": "mirroring", "supportedStreams": [{
        "index": 0, "type": "video_source", "codecName": "h264", "rtpProfile": "cast",
        "rtpPayloadType": PAYLOAD_TYPE, "ssrc": SSRC, "maxFrameRate": "30000/1000",
        "timeBase": "1/90000", "maxBitRate": args.bitrate * 1000, "targetDelay": args.delay,
        "aesKey": key.hex(), "aesIvMask": iv_mask.hex(), "receiverRtcpEventLog": False,
        "resolutions": [{"width": w, "height": h}]}]}}

    ch = Channel(args.host)
    t_launch = time.time()
    answer = launch(ch, offer)
    print(f"ANSWER after {time.time() - t_launch:.2f}s:", json.dumps(answer["answer"]))
    if 0 not in answer["answer"].get("sendIndexes", []):
        raise SystemExit("video stream refused")
    sender = Sender(args.host, answer["answer"]["udpPort"], key, iv_mask)
    stop = threading.Event()
    threading.Thread(target=control_loop, args=(ch, stop), daemon=True).start()

    Gst.init(None)
    src = args.src or (f"videotestsrc is-live=true pattern=ball ! video/x-raw,width={w},height={h},framerate=30/1"
                       " ! textoverlay name=clock font-desc=\"Sans Bold 64\" valignment=center halignment=center")
    if args.preview:
        # Same frames on the PC screen: a photo of both screens gives the delay.
        src += " ! tee name=t t. ! queue max-size-buffers=1 leaky=downstream ! videoconvert ! autovideosink sync=false t."
    pipe = Gst.parse_launch(
        f"{src} ! videoconvert ! queue max-size-buffers=1 leaky=downstream"
        f" ! x264enc name=enc tune=zerolatency speed-preset=ultrafast bitrate={args.bitrate} key-int-max=60 bframes=0"
        " ! video/x-h264,profile=high ! h264parse config-interval=-1"
        " ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=sink sync=false max-buffers=2 drop=false")
    clock_overlay = pipe.get_by_name("clock")
    if clock_overlay:
        def stamp(pad, info):
            clock_overlay.set_property("text", datetime.now().strftime("%H:%M:%S.%f")[:-3])
            return Gst.PadProbeReturn.OK
        clock_overlay.get_static_pad("video_sink").add_probe(Gst.PadProbeType.BUFFER, stamp)
    enc = pipe.get_by_name("enc")

    def request_key_frame():
        enc.send_event(Gst.Event.new_custom(Gst.EventType.CUSTOM_DOWNSTREAM, Gst.Structure.new_from_string(
            "GstForceKeyUnit, all-headers=(boolean)true")))

    def rtcp_loop():
        sender.sock.settimeout(0.5)
        while not stop.is_set():
            try:
                buf = sender.sock.recv(2048)
            except socket.timeout:
                continue
            sender.on_rtcp(buf, request_key_frame)

    pipe.set_state(Gst.State.PLAYING)
    pipe.get_state(Gst.CLOCK_TIME_NONE)
    clk = pipe.get_clock()
    sender.wall0 = time.time() - (clk.get_time() - pipe.get_base_time()) / 1e9
    sender.send_sr()
    threading.Thread(target=rtcp_loop, daemon=True).start()

    sink = pipe.get_by_name("sink")
    t_end = time.time() + args.seconds
    last_sr = last_log = time.time()
    frames = 0
    lag_sum = 0.0
    while time.time() < t_end and not stop.is_set():
        sample = sink.emit("try-pull-sample", 100 * Gst.MSECOND)
        now = time.time()
        if sample:
            buf = sample.get_buffer()
            # x264enc shifts PTS by 1000 h; the segment maps it back to running time.
            running = sample.get_segment().to_running_time(Gst.Format.TIME, buf.pts)
            capture = sender.wall0 + running / 1e9
            data = buf.extract_dup(0, buf.get_size())
            sender.send_frame(data, capture, not buf.has_flags(Gst.BufferFlags.DELTA_UNIT))
            frames += 1
            lag_sum += now - capture
        if now - last_sr >= SR_INTERVAL:
            sender.send_sr()
            last_sr = now
        if now - last_log >= 2:
            s = sender.stats
            print(f"frame {sender.frame_id} ckpt {s['ckpt']} delay {s['delay']}ms rr {s['rr']} "
                  f"nack {s['nack_pkts']} pli {s['pli']} capture->send {1000 * lag_sum / max(frames, 1):.0f}ms "
                  f"{8 * sender.octets / (now - sender.wall0) / 1000:.0f}kbit/s")
            last_log, frames, lag_sum = now, 0, 0.0
    stop.set()
    pipe.set_state(Gst.State.NULL)
    ch.send("receiver-0", NS_RECEIVER, {"type": "STOP", "requestId": 0})
    time.sleep(0.5)


if __name__ == "__main__":
    main()
