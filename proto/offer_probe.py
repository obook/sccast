#!/usr/bin/env python3
"""Launch the Cast mirroring receiver and print its ANSWER to an OFFER.

Usage: offer_probe.py DEVICE_IP [APP_ID]
No dependency outside the standard library: CASTV2 protobuf is encoded by hand.
"""
import json
import os
import socket
import ssl
import struct
import sys
import time

MIRRORING_APP = "0F5096E8"
NS_CONN = "urn:x-cast:com.google.cast.tp.connection"
NS_HEARTBEAT = "urn:x-cast:com.google.cast.tp.heartbeat"
NS_RECEIVER = "urn:x-cast:com.google.cast.receiver"
NS_WEBRTC = "urn:x-cast:com.google.cast.webrtc"
SENDER = "sender-0"


def varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)


def field_str(num, s):
    b = s.encode()
    return varint(num << 3 | 2) + varint(len(b)) + b


def encode(src, dst, ns, payload):
    # CastMessage: protocol_version=0, source_id, destination_id, namespace, payload_type=STRING, payload_utf8.
    msg = (varint(1 << 3) + varint(0) + field_str(2, src) + field_str(3, dst)
           + field_str(4, ns) + varint(5 << 3) + varint(0) + field_str(6, payload))
    return struct.pack(">I", len(msg)) + msg


def decode(buf):
    fields, i = {}, 0
    while i < len(buf):
        key, i = read_varint(buf, i)
        num, wire = key >> 3, key & 7
        if wire == 0:
            val, i = read_varint(buf, i)
        else:
            length, i = read_varint(buf, i)
            val, i = buf[i:i + length], i + length
        fields[num] = val
    return {k: (v.decode(errors="replace") if isinstance(v, bytes) else v) for k, v in fields.items()}


def read_varint(buf, i):
    n = shift = 0
    while True:
        b = buf[i]
        i += 1
        n |= (b & 0x7F) << shift
        shift += 7
        if not b & 0x80:
            return n, i


class Channel:
    def __init__(self, host):
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        self.sock = ctx.wrap_socket(socket.create_connection((host, 8009), timeout=5))
        self.req = 1

    def send(self, dst, ns, obj):
        if "requestId" in obj:
            obj["requestId"] = self.req
            self.req += 1
        self.sock.sendall(encode(SENDER, dst, ns, json.dumps(obj)))

    def recv(self, timeout):
        """Return the next non heartbeat message as (namespace, dict), or None on timeout."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.sock.settimeout(max(0.1, deadline - time.time()))
            try:
                head = self._read(4)
            except socket.timeout:
                return None
            m = decode(self._read(struct.unpack(">I", head)[0]))
            body = json.loads(m.get(6, "{}") or "{}")
            if m.get(4) == NS_HEARTBEAT:
                if body.get("type") == "PING":
                    self.send(m[2], NS_HEARTBEAT, {"type": "PONG"})
                continue
            return m.get(4), m.get(2), body
        return None

    def _read(self, n):
        data = b""
        while len(data) < n:
            chunk = self.sock.recv(n - len(data))
            if not chunk:
                raise ConnectionError("device closed the connection")
            data += chunk
        return data


def video_stream(index, codec, ssrc):
    return {
        "index": index, "type": "video_source", "codecName": codec,
        "rtpProfile": "cast", "rtpPayloadType": 96, "ssrc": ssrc,
        "maxFrameRate": "30000/1000", "timeBase": "1/90000", "maxBitRate": 5000000,
        "aesKey": os.urandom(16).hex(), "aesIvMask": os.urandom(16).hex(),
        "targetDelay": 200, "receiverRtcpEventLog": False,
        "resolutions": [{"width": 1920, "height": 1080}],
    }


def main():
    host = sys.argv[1]
    app = sys.argv[2] if len(sys.argv) > 2 else MIRRORING_APP
    ch = Channel(host)
    ch.send("receiver-0", NS_CONN, {"type": "CONNECT"})
    ch.send("receiver-0", NS_RECEIVER, {"type": "LAUNCH", "appId": app, "requestId": 0})
    transport = None
    t0 = time.time()
    while time.time() - t0 < 30 and not transport:
        r = ch.recv(30)
        if not r:
            break
        ns, src, body = r
        print("<-", src, ns, json.dumps(body)[:600])
        status = body.get("status")
        for a in status.get("applications", []) if isinstance(status, dict) else []:
            if a.get("appId") == app:
                transport = a["transportId"]
                print("namespaces:", [n["name"] for n in a.get("namespaces", [])])
    if not transport:
        sys.exit("mirroring app did not start")

    ch.send(transport, NS_CONN, {"type": "CONNECT"})
    offer = {"type": "OFFER", "seqNum": 1, "offer": {"castMode": "mirroring", "supportedStreams": [
        video_stream(0, "h264", 100001),
        video_stream(1, "vp8", 100002),
        {"index": 2, "type": "audio_source", "codecName": "opus", "rtpProfile": "cast",
         "rtpPayloadType": 96, "ssrc": 100003, "bitRate": 128000, "timeBase": "1/48000",
         "channels": 2, "aesKey": os.urandom(16).hex(), "aesIvMask": os.urandom(16).hex(),
         "targetDelay": 200, "receiverRtcpEventLog": False},
    ]}}
    sent = time.time()
    ch.send(transport, NS_WEBRTC, offer)
    while True:
        r = ch.recv(15)
        if not r:
            print("no more messages")
            break
        ns, src, body = r
        print(f"<- +{time.time() - sent:.2f}s", src, ns, json.dumps(body))
        if body.get("type") == "ANSWER":
            break
    ch.send("receiver-0", NS_RECEIVER, {"type": "STOP", "requestId": 0})
    time.sleep(1)


if __name__ == "__main__":
    main()
