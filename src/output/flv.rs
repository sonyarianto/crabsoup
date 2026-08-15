//! FLV container muxing for the RTMP output (Part H5).
//!
//! Pure Rust byte assembly — no FFI. librtmp only transports bytes; the
//! FLV framing (header, tags, AMF0 metadata, raw-AAC + AVC payload headers)
//! lives here so the whole stream can be unit-tested without a server and
//! the same byte stream that hits the network can be written to a file and
//! probed with ffprobe.

/// FLV tag types.
pub const TAG_AUDIO: u8 = 8;
pub const TAG_VIDEO: u8 = 9;
pub const TAG_SCRIPT: u8 = 18;

/// `SoundFormat=10 (AAC) | SoundRate=3 (44 kHz) | SoundSize=1 (16-bit) |
/// SoundType=1 (stereo)` — the FLV audio tag header byte for AAC.
pub const AAC_HEADER: u8 = 0xAF;

/// `FrameType=1 (keyframe) | CodecID=7 (AVC)` and the inter-frame variant.
pub const VIDEO_KEYFRAME: u8 = 0x17;
pub const VIDEO_INTER: u8 = 0x27;

/// Full FLV header (9 bytes) plus the mandatory leading PreviousTagSize0.
/// `has_video` toggles the audio+video / audio-only flags byte.
pub fn header(has_video: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(13);
    out.extend_from_slice(b"FLV");
    out.push(0x01); // version
    out.push(if has_video { 0x05 } else { 0x04 });
    out.extend_from_slice(&[0, 0, 0, 9]); // header size
    out.extend_from_slice(&[0, 0, 0, 0]); // previous tag size 0
    out
}

/// One FLV tag: 11-byte header (type, 24-bit size, 24-bit ms timestamp,
/// stream id 0) + payload + 32-bit previous-tag-size.
pub fn tag(tag_type: u8, ts_ms: u32, data: &[u8]) -> Vec<u8> {
    let size = data.len() as u32;
    let mut out = Vec::with_capacity(11 + size as usize + 4);
    out.push(tag_type);
    out.extend_from_slice(&size.to_be_bytes()[1..]);
    out.extend_from_slice(&ts_ms.to_be_bytes()[1..]);
    out.push(0); // extended timestamp (never needed: live streams are short)
    out.extend_from_slice(&[0, 0, 0]); // stream id
    out.extend_from_slice(data);
    out.extend_from_slice(&(size + 11).to_be_bytes());
    out
}

/// AAC audio tag: header byte + `AACPacketType` (0 sequence header,
/// 1 raw access unit) + payload. `aac` is the ASC for a sequence header,
/// a raw access unit otherwise.
pub fn audio_tag(ts_ms: u32, sequence_header: bool, aac: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + aac.len());
    payload.push(AAC_HEADER);
    payload.push(if sequence_header { 0 } else { 1 });
    payload.extend_from_slice(aac);
    tag(TAG_AUDIO, ts_ms, &payload)
}

/// AVC video tag: header byte + `AVCPacketType=1` (NALU) + composition
/// time (0: no B-frames, PTS == DTS) + length-prefixed access unit.
pub fn video_tag(ts_ms: u32, keyframe: bool, avcc: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(5 + avcc.len());
    payload.push(if keyframe {
        VIDEO_KEYFRAME
    } else {
        VIDEO_INTER
    });
    payload.push(1); // AVC NALU
    payload.extend_from_slice(&[0, 0, 0]); // composition time
    payload.extend_from_slice(avcc);
    tag(TAG_VIDEO, ts_ms, &payload)
}

/// AVC sequence header tag: `AVCPacketType=0` + the
/// AVCDecoderConfigurationRecord. Timestamp 0 by convention.
pub fn video_sequence_header(avcdcr: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(5 + avcdcr.len());
    payload.push(VIDEO_KEYFRAME);
    payload.push(0); // AVC sequence header
    payload.extend_from_slice(&[0, 0, 0]);
    payload.extend_from_slice(avcdcr);
    tag(TAG_VIDEO, 0, &payload)
}

/// The `onMetaData` script tag (AMF0 ECMA array), published before the
/// first A/V tag so players can size their canvas.
pub fn metadata_tag(
    width: u32,
    height: u32,
    fps: f64,
    has_video: bool,
    audio_bitrate: u32,
) -> Vec<u8> {
    let mut data = Vec::new();
    data.push(0x02); // AMF0 string "onMetaData"
    data.extend_from_slice(&10u16.to_be_bytes());
    data.extend_from_slice(b"onMetaData");
    data.push(0x08); // ECMA array
    let count_pos = data.len();
    data.extend_from_slice(&[0, 0, 0, 0]); // patched below
    let mut count = 0u32;
    let mut num = |name: &str, value: f64, data: &mut Vec<u8>| {
        data.push(0x02);
        data.extend_from_slice(&(name.len() as u16).to_be_bytes());
        data.extend_from_slice(name.as_bytes());
        data.push(0x00); // number
        data.extend_from_slice(&value.to_be_bytes());
        count += 1;
    };
    num("duration", 0.0, &mut data);
    if has_video {
        num("width", width as f64, &mut data);
        num("height", height as f64, &mut data);
        num("framerate", fps, &mut data);
        num("videocodecid", 7.0, &mut data);
    }
    num("audiocodecid", 10.0, &mut data);
    num("audiodatarate", audio_bitrate as f64 / 1000.0, &mut data);
    data[count_pos..count_pos + 4].copy_from_slice(&count.to_be_bytes());
    data.extend_from_slice(&[0, 0, 9]); // end of ECMA array
    tag(TAG_SCRIPT, 0, &data)
}

/// Repack an Annex-B access unit (start-code-prefixed NALs) into FLV's
/// length-prefixed form (4-byte big-endian lengths, `lengthSizeMinusOne=3`).
pub fn avcc_nalus(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 8);
    for nalu in split_annexb(data) {
        out.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        out.extend_from_slice(nalu);
    }
    out
}

/// Extract the SPS (NAL type 7) and PPS (type 8) from an Annex-B access
/// unit — needed to build the AVCDecoderConfigurationRecord.
pub fn parameter_sets(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    for nalu in split_annexb(data) {
        let Some(&kind) = nalu.first() else {
            continue;
        };
        match kind & 0x1f {
            7 => sps = Some(nalu.to_vec()),
            8 => pps = Some(nalu.to_vec()),
            _ => {}
        }
        if sps.is_some() && pps.is_some() {
            break;
        }
    }
    match (sps, pps) {
        (Some(sps), Some(pps)) => Some((sps, pps)),
        _ => None,
    }
}

/// The AVCDecoderConfigurationRecord: profile/level from the SPS plus the
/// length-prefixed SPS/PPS NALs (configurationVersion 1, 4-byte lengths).
pub fn avcdcr(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.push(sps[1]); // AVCProfileIndication
    out.push(sps[2]); // profile_compatibility
    out.push(sps[3]); // AVCLevelIndication
    out.push(0xFF);
    out.push(0xE1); // lengthSizeMinusOne = 3 (4-byte lengths)
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numPPS
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

/// Split an Annex-B buffer (start codes 00 00 00 01 or 00 00 01) into its
/// NAL unit payloads.
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i < data.len() {
        let four = i + 3 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1;
        let three = i + 2 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if four || three {
            let code_len = if four { 4 } else { 3 };
            if let Some(s) = start.take() {
                nals.push(&data[s..i]);
            }
            i += code_len;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        nals.push(&data[s..]);
    }
    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_flv_spec() {
        let h = header(true);
        assert_eq!(&h[..9], &[b'F', b'L', b'V', 0x01, 0x05, 0, 0, 0, 9]);
        assert_eq!(&h[9..13], &[0, 0, 0, 0], "leading PreviousTagSize0");
        assert_eq!(header(false)[4], 0x04, "audio-only flags");
    }

    #[test]
    fn tag_layout_is_byte_exact() {
        let t = tag(TAG_AUDIO, 1234, &[1, 2, 3]);
        assert_eq!(t.len(), 11 + 3 + 4);
        assert_eq!(t[0], TAG_AUDIO);
        assert_eq!(&t[1..4], &[0, 0, 3], "24-bit data size");
        assert_eq!(&t[4..8], &[0, 4, 0xD2, 0], "24-bit ms timestamp 1234");
        assert_eq!(&t[8..11], &[0, 0, 0], "stream id 0");
        assert_eq!(&t[11..14], &[1, 2, 3]);
        assert_eq!(&t[14..18], &[0, 0, 0, 14], "previous tag size = 11 + 3");
        // Timestamps that would need the extended byte: 24-bit wraps at 4.6
        // hours; live streams never reach it, and the byte stays 0.
        assert_eq!(t[7], 0);
    }

    #[test]
    fn audio_tag_headers() {
        let seq = audio_tag(0, true, &[0x12, 0x10]);
        assert_eq!(seq[11], AAC_HEADER);
        assert_eq!(seq[12], 0, "sequence header packet type");
        assert_eq!(&seq[13..15], &[0x12, 0x10]);
        let raw = audio_tag(23, false, &[0xAA]);
        assert_eq!(raw[12], 1, "raw access unit packet type");
        assert_eq!(raw[13], 0xAA);
    }

    #[test]
    fn video_tags_and_sequence_header() {
        let key = video_tag(40, true, &[0, 0, 0, 1, 0x65, 0x88]);
        assert_eq!(key[11], VIDEO_KEYFRAME);
        assert_eq!(key[12], 1, "NALU packet type");
        assert_eq!(&key[13..16], &[0, 0, 0], "composition time");
        let inter = video_tag(40, false, &[]);
        assert_eq!(inter[11], VIDEO_INTER);
        let seq = video_sequence_header(&[1, 2]);
        assert_eq!(seq[11], VIDEO_KEYFRAME);
        assert_eq!(seq[12], 0, "sequence header packet type");
        assert_eq!(seq[13], 0, "sequence header timestamp is 0");
    }

    #[test]
    fn metadata_tag_contains_known_properties() {
        let m = metadata_tag(320, 240, 25.0, true, 128_000);
        assert_eq!(m[0], TAG_SCRIPT);
        let data = &m[11..m.len() - 4];
        let text = String::from_utf8_lossy(data);
        assert!(text.contains("onMetaData"));
        assert!(text.contains("width"));
        assert!(text.contains("height"));
        assert!(text.contains("videocodecid"));
        assert!(text.contains("audiocodecid"));
        // ECMA array count lands between the 4 reserved bytes.
        let count = u32::from_be_bytes(data[14..18].try_into().unwrap());
        assert_eq!(
            count, 7,
            "duration, width, height, framerate, videocodecid, audiocodecid, audiodatarate"
        );
    }

    #[test]
    fn annexb_split_and_avcc_repack_round_trip() {
        let sps = [0x67, 0x42, 0x40, 0x1f];
        let pps = [0x68, 0xCE];
        let idr = [0x65, 0x11, 0x22];
        let au = [0, 0, 0, 1]
            .iter()
            .chain(&sps)
            .chain(&[0, 0, 0, 1])
            .chain(&pps)
            .chain(&[0, 0, 0, 1])
            .chain(&idr)
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(split_annexb(&au).len(), 3);

        let (got_sps, got_pps) = parameter_sets(&au).expect("parameter sets found");
        assert_eq!(got_sps, sps);
        assert_eq!(got_pps, pps);
        assert!(parameter_sets(&[0, 0, 0, 1, 0x65]).is_none());

        let avcc = avcc_nalus(&au);
        let mut i = 0;
        for expect in [&sps[..], &pps[..], &idr[..]] {
            let n = u32::from_be_bytes(avcc[i..i + 4].try_into().unwrap()) as usize;
            assert_eq!(n, expect.len());
            assert_eq!(&avcc[i + 4..i + 4 + n], expect);
            i += 4 + n;
        }

        let dcr = avcdcr(&sps, &pps);
        assert_eq!(dcr[0], 1);
        assert_eq!(dcr[1], 0x42, "profile from SPS");
        assert_eq!(dcr[3], 0x1f, "level from SPS");
        assert_eq!(dcr[5], 0xE1, "4-byte NALU lengths");
        assert_eq!(&dcr[6..8], &[0, 4], "SPS length (4 bytes)");
        assert_eq!(&dcr[8..12], &sps);
        assert_eq!(dcr[12], 1, "numPPS");
        assert_eq!(&dcr[13..15], &[0, 2], "PPS length");
        assert_eq!(&dcr[15..], &pps);
    }

    #[test]
    fn three_byte_start_codes_are_split_too() {
        let au = [0, 0, 1, 0x65, 0x01, 0, 0, 1, 0x41, 0x02];
        let nals = split_annexb(&au);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x65, 0x01]);
        assert_eq!(nals[1], &[0x41, 0x02]);
    }
}
