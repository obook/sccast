//! offer.rs
//! Start of a mirroring session: sccast sends an OFFER (streams, codecs, keys) on the Cast
//! control channel, the TV replies with an ANSWER (UDP port, streams it accepts).
//! mod.rs then sends the streams to that port.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use rust_cast::channels::connection::ConnectionResponse;
use rust_cast::channels::heartbeat::HeartbeatResponse;
use rust_cast::message_manager::CastMessagePayload;
use rust_cast::{CastDevice, ChannelMessage};
use serde_json::{json, Value};

use super::rtp::{hex, Keys};
use super::{AUDIO_BITRATE, AUDIO_SSRC, H264_PAYLOAD, NAMESPACE, OPUS_PAYLOAD, TARGET_DELAY_MS, VIDEO_SSRC};
use crate::cli::Stream;

const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// RTP extension that lets the sender change the playout delay during the session.
const ADAPTIVE_DELAY: &str = "adaptive_playout_delay";

/// OFFER number `seq` for the video stream, and the audio one unless `audio` is None.
pub fn offer(stream: &Stream, seq: u32, video: &Keys, audio: Option<&Keys>) -> Value {
    let (width, height) = stream.resolution;
    let mut streams = vec![json!({
        "index": 0, "type": "video_source", "codecName": "h264", "rtpProfile": "cast",
        "rtpPayloadType": H264_PAYLOAD, "ssrc": VIDEO_SSRC, "maxFrameRate": format!("{}/1", stream.fps),
        "timeBase": "1/90000", "maxBitRate": stream.bitrate * 1000, "targetDelay": TARGET_DELAY_MS,
        "aesKey": hex(&video.key), "aesIvMask": hex(&video.iv_mask), "receiverRtcpEventLog": false,
        "resolutions": [{"width": width, "height": height}], "rtpExtensions": [ADAPTIVE_DELAY],
    })];
    if let Some(audio) = audio {
        streams.push(json!({
            "index": 1, "type": "audio_source", "codecName": "opus", "rtpProfile": "cast",
            "rtpPayloadType": OPUS_PAYLOAD, "ssrc": AUDIO_SSRC, "bitRate": AUDIO_BITRATE,
            "timeBase": "1/48000", "channels": 2, "targetDelay": TARGET_DELAY_MS,
            "aesKey": hex(&audio.key), "aesIvMask": hex(&audio.iv_mask), "receiverRtcpEventLog": false,
            "rtpExtensions": [ADAPTIVE_DELAY],
        }));
    }
    json!({"type": "OFFER", "seqNum": seq, "offer": {"castMode": "mirroring", "supportedStreams": streams}})
}

/// What the receiver accepted.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub port: u16,
    pub video: bool,
    pub audio: bool,
    /// Every accepted stream takes a new playout delay in its packets: audio and video
    /// change it together, or lip sync would drift.
    pub adaptive: bool,
}

/// A refusal (`result` not "ok") reads as no stream accepted: the caller tries a smaller video.
fn parse_answer(answer: &Value) -> anyhow::Result<Answer> {
    if answer["result"] != "ok" {
        tracing::info!(error = %answer["error"], "the TV refused the OFFER");
        return Ok(Answer { port: 0, video: false, audio: false, adaptive: false });
    }
    let body = &answer["answer"];
    let port = body["udpPort"].as_u64().and_then(|p| u16::try_from(p).ok()).context("ANSWER without udpPort")?;
    let accepted = |index| body["sendIndexes"].as_array().is_some_and(|a| a.contains(&json!(index)));
    // One list of extensions per accepted stream.
    let adaptive = body["rtpExtensions"].as_array().is_some_and(|streams| {
        !streams.is_empty()
            && streams.iter().all(|e| e.as_array().is_some_and(|e| e.contains(&json!(ADAPTIVE_DELAY))))
    });
    Ok(Answer { port, video: accepted(0), audio: accepted(1), adaptive })
}

/// Wait for the ANSWER to the OFFER, answering the device's pings meanwhile.
/// None when `stop` is set first.
pub fn wait_answer(cast: &CastDevice<'static>, stop: &AtomicBool) -> anyhow::Result<Option<Answer>> {
    let started = Instant::now();
    while started.elapsed() < ANSWER_TIMEOUT {
        if stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let message = match cast.receive() {
            Ok(message) => message,
            Err(rust_cast::errors::Error::Parsing(_) | rust_cast::errors::Error::Serialization(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        match message {
            ChannelMessage::Heartbeat(HeartbeatResponse::Ping) => cast.heartbeat.pong()?,
            ChannelMessage::Connection(ConnectionResponse::Close) => bail!("the TV closed the mirroring session"),
            ChannelMessage::Raw(message) if message.namespace == NAMESPACE => {
                let CastMessagePayload::String(text) = message.payload else { continue };
                // Like rust_cast's own parse errors: skip what cannot be read.
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                if value["type"] == "ANSWER" {
                    // Whole: it may hold limits (constraints, display) worth using later.
                    tracing::info!(answer = %text, "ANSWER");
                    return parse_answer(&value).map(Some);
                }
            }
            _ => {}
        }
    }
    bail!("no answer from the mirroring receiver")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_offer_and_reads_answer() {
        let keys = Keys { key: [0x11; 16], iv_mask: [0x22; 16] };
        let stream = Stream {
            resolution: (1280, 720),
            fps: 30,
            bitrate: 2500,
            port: 8090,
            no_audio: false,
            format: crate::cli::Format::Mirror,
        };
        let offer = offer(&stream, 2, &keys, Some(&keys));
        assert_eq!(offer["seqNum"], 2);
        let streams = offer["offer"]["supportedStreams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0]["codecName"], "h264");
        assert_eq!(streams[0]["maxBitRate"], 2_500_000);
        assert_eq!(streams[0]["aesKey"], "11111111111111111111111111111111");
        assert_eq!(streams[1]["ssrc"], AUDIO_SSRC);

        // The Sharp TV's answer, video only.
        let answer: Value = serde_json::from_str(
            r#"{"answer":{"castMode":"mirroring","rtpExtensions":[[]],"sendIndexes":[0],"ssrcs":[2],"udpPort":36026},"result":"ok","seqNum":1,"type":"ANSWER"}"#,
        )
        .unwrap();
        assert_eq!(parse_answer(&answer).unwrap(), Answer { port: 36026, video: true, audio: false, adaptive: false });
        assert_eq!(streams[0]["rtpExtensions"], json!([ADAPTIVE_DELAY]));
        let both = json!({"result": "ok", "answer": {"udpPort": 1, "sendIndexes": [0, 1],
            "rtpExtensions": [[ADAPTIVE_DELAY], [ADAPTIVE_DELAY]]}});
        assert!(parse_answer(&both).unwrap().adaptive);
        let video_only = json!({"result": "ok", "answer": {"udpPort": 1, "sendIndexes": [0, 1],
            "rtpExtensions": [[ADAPTIVE_DELAY], []]}});
        assert!(!parse_answer(&video_only).unwrap().adaptive);
        let refused = parse_answer(&json!({"result": "error", "error": {"code": 2}})).unwrap();
        assert!(!refused.video && !refused.audio);
    }
}
