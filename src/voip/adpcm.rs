//! IMA ADPCM, 4 bits per sample: 16 kHz mono voice in about 64 kbit/s, in plain
//! Rust (no C codec to build). Every frame starts with the encoder state, so a
//! frame decodes on its own and a lost or late one doesn't garble the next.
//!
//! Frame: predictor (i16 LE), step index (u8), 0, then two samples per byte,
//! low nibble first.

const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107,
    118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
    1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894,
    6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];
const INDEX: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Default, Clone, Copy)]
pub struct Encoder {
    predictor: i32,
    index: i32,
}

impl Encoder {
    pub fn encode(&mut self, samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + samples.len().div_ceil(2));
        out.extend_from_slice(&(self.predictor as i16).to_le_bytes());
        out.push(self.index as u8);
        out.push(0);
        let mut pending: Option<u8> = None;
        for &s in samples {
            let nibble = self.step(s as i32);
            match pending.take() {
                None => pending = Some(nibble),
                Some(lo) => out.push(lo | (nibble << 4)),
            }
        }
        if let Some(lo) = pending {
            out.push(lo);
        }
        out
    }

    fn step(&mut self, sample: i32) -> u8 {
        let step = STEPS[self.index as usize];
        let mut diff = sample - self.predictor;
        let mut nibble = 0u8;
        if diff < 0 {
            nibble = 8;
            diff = -diff;
        }
        let mut delta = step >> 3;
        if diff >= step {
            nibble |= 4;
            diff -= step;
            delta += step;
        }
        if diff >= step >> 1 {
            nibble |= 2;
            diff -= step >> 1;
            delta += step >> 1;
        }
        if diff >= step >> 2 {
            nibble |= 1;
            delta += step >> 2;
        }
        self.predictor = if nibble & 8 != 0 { self.predictor - delta } else { self.predictor + delta }.clamp(-32768, 32767);
        self.index = (self.index + INDEX[nibble as usize]).clamp(0, 88);
        nibble
    }
}

/// Decodes one frame; `None` if it's malformed.
pub fn decode(frame: &[u8]) -> Option<Vec<i16>> {
    if frame.len() < 4 || frame[2] > 88 {
        return None;
    }
    let mut predictor = i16::from_le_bytes([frame[0], frame[1]]) as i32;
    let mut index = frame[2] as i32;
    let mut out = Vec::with_capacity((frame.len() - 4) * 2);
    for &byte in &frame[4..] {
        for nibble in [byte & 0x0F, byte >> 4] {
            let step = STEPS[index as usize];
            let mut delta = step >> 3;
            if nibble & 4 != 0 {
                delta += step;
            }
            if nibble & 2 != 0 {
                delta += step >> 1;
            }
            if nibble & 1 != 0 {
                delta += step >> 2;
            }
            predictor = if nibble & 8 != 0 { predictor - delta } else { predictor + delta }.clamp(-32768, 32767);
            index = (index + INDEX[nibble as usize]).clamp(0, 88);
            out.push(predictor as i16);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_close() {
        let input: Vec<i16> = (0..320).map(|i| ((i as f32 * 0.07).sin() * 9000.0) as i16).collect();
        let mut enc = Encoder::default();
        // A frame or two in, the step size has adapted to the signal.
        enc.encode(&input);
        let frame = enc.encode(&input);
        assert_eq!(frame.len(), 4 + 160);
        let out = decode(&frame).unwrap();
        assert_eq!(out.len(), 320);
        let err: f64 = input.iter().zip(&out).map(|(a, b)| ((*a as f64) - (*b as f64)).abs()).sum::<f64>() / 320.0;
        assert!(err < 400.0, "mean error {err}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(&[1, 2]).is_none());
        assert!(decode(&[0, 0, 200, 0, 1]).is_none());
    }
}
