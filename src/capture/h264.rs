//! H.264 helpers shared by both native encoders: the byte stream the page's
//! decoder takes (Annex B, start codes), the codec string it's configured
//! with, and making sure every key frame carries its parameter sets so a
//! watcher who joins late can start from it.
//!
//! Each platform's encoder uses part of this (Windows gets Annex B already,
//! VideoToolbox gives length-prefixed NAL units).
#![cfg_attr(not(test), allow(dead_code))]

/// NAL unit types we care about.
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

const START: [u8; 4] = [0, 0, 0, 1];

/// The NAL units of an Annex B stream (without their start codes).
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                // A 4-byte start code's leading zero belongs to it, not the NAL before.
                let mut end = i;
                if end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                out.push(&data[s..end]);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            out.push(&data[s..]);
        }
    }
    out
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1f).unwrap_or(0)
}

/// Length-prefixed NAL units (AVCC, what VideoToolbox makes) as Annex B.
pub fn avcc_to_annexb(data: &[u8], len_size: usize, out: &mut Vec<u8>) {
    let mut i = 0;
    while i + len_size <= data.len() {
        let mut n = 0usize;
        for b in &data[i..i + len_size] {
            n = (n << 8) | *b as usize;
        }
        i += len_size;
        if n == 0 || i + n > data.len() {
            break;
        }
        out.extend_from_slice(&START);
        out.extend_from_slice(&data[i..i + n]);
        i += n;
    }
}

/// The WebCodecs codec string for a stream, from its SPS ("avc1.640028").
pub fn codec_string(sps: &[u8]) -> Option<String> {
    if nal_type(sps) != NAL_SPS || sps.len() < 4 {
        return None;
    }
    Some(format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// Remembers the stream's SPS and PPS, and puts them in front of any key frame
/// that comes without them (some encoders only send them once).
#[derive(Default)]
pub struct ParamSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSets {
    /// Parameter sets from out of band (an encoder's sequence header), in
    /// either Annex B or as bare NALs.
    pub fn learn(&mut self, data: &[u8]) {
        for nal in nal_units(data) {
            match nal_type(nal) {
                NAL_SPS => self.sps = Some(nal.to_vec()),
                NAL_PPS => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
    }

    pub fn set(&mut self, sps: Vec<u8>, pps: Vec<u8>) {
        self.sps = Some(sps);
        self.pps = Some(pps);
    }

    pub fn codec(&self) -> Option<String> {
        self.sps.as_deref().and_then(codec_string)
    }

    /// The frame, with SPS and PPS first if it's a key frame that lacks them.
    /// Also learns any it carries.
    pub fn fix(&mut self, frame: Vec<u8>, key: bool) -> Vec<u8> {
        let mut has_sps = false;
        for nal in nal_units(&frame) {
            match nal_type(nal) {
                NAL_SPS => {
                    has_sps = true;
                    self.sps = Some(nal.to_vec());
                }
                NAL_PPS => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        if !key || has_sps {
            return frame;
        }
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else { return frame };
        let mut out = Vec::with_capacity(frame.len() + sps.len() + pps.len() + 8);
        out.extend_from_slice(&START);
        out.extend_from_slice(sps);
        out.extend_from_slice(&START);
        out.extend_from_slice(pps);
        out.extend_from_slice(&frame);
        out
    }
}

/// Is this Annex B frame a key frame (has an IDR slice)?
pub fn is_idr(data: &[u8]) -> bool {
    nal_units(data).iter().any(|n| nal_type(n) == NAL_IDR)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 6] = [0x67, 0x64, 0x00, 0x28, 0xac, 0xd9];
    const PPS: [u8; 4] = [0x68, 0xeb, 0xe3, 0xcb];

    #[test]
    fn splits_both_start_code_lengths() {
        let mut s = vec![0, 0, 0, 1];
        s.extend_from_slice(&SPS);
        s.extend_from_slice(&[0, 0, 1]);
        s.extend_from_slice(&PPS);
        s.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2, 3]);
        let n = nal_units(&s);
        assert_eq!(n, vec![&SPS[..], &PPS[..], &[0x65, 1, 2, 3][..]]);
        assert!(is_idr(&s));
    }

    #[test]
    fn codec_string_from_sps() {
        assert_eq!(codec_string(&SPS).as_deref(), Some("avc1.640028"));
        assert_eq!(codec_string(&PPS), None);
    }

    #[test]
    fn avcc_becomes_annexb() {
        let avcc = [0, 0, 0, 4, 0x65, 9, 9, 9, 0, 0, 0, 2, 0x41, 7];
        let mut out = Vec::new();
        avcc_to_annexb(&avcc, 4, &mut out);
        assert_eq!(out, vec![0, 0, 0, 1, 0x65, 9, 9, 9, 0, 0, 0, 1, 0x41, 7]);
    }

    #[test]
    fn key_frames_get_parameter_sets() {
        let mut p = ParamSets::default();
        p.set(SPS.to_vec(), PPS.to_vec());
        let idr = vec![0, 0, 0, 1, 0x65, 1, 2];
        let fixed = p.fix(idr.clone(), true);
        let n = nal_units(&fixed);
        assert_eq!(n.len(), 3);
        assert_eq!(nal_type(n[0]), NAL_SPS);
        assert_eq!(nal_type(n[1]), NAL_PPS);
        // Delta frames and key frames that have them are left alone.
        let delta = vec![0, 0, 0, 1, 0x41, 1];
        assert_eq!(p.fix(delta.clone(), false), delta);
        assert_eq!(p.fix(fixed.clone(), true), fixed);
    }
}
