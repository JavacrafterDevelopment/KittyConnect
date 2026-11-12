//! Room protocol framing. Everything in a room (names, chat, voice) travels as
//! these frames over one TCP connection per member, and that connection is
//! carried by the TailCat tunnel: WireGuard-encrypted end to end.
//!
//!   [kind: u8][length: u32 BE][payload]
//!
//! kind 1: a JSON control message ({"t": "hello" | "chat" | ...}).
//! kind 2: voice. Member → room: [seq u32][ADPCM frame].
//!         Room → member: [from u16][seq u32][ADPCM frame].
//! kind 3: a file chunk. Member → room: [upload id u32][bytes].
//!         Room → member: [attachment id u32][bytes].
//! kind 4: screen video. Member → room: [flags u8][timestamp µs u64][encoded chunk].
//!         Room → watcher: [from u16][flags u8][timestamp u64][encoded chunk].
//!         flags bit 0: key frame.

use std::io::{self, Read, Write};

pub const JSON: u8 = 1;
pub const AUDIO: u8 = 2;
pub const FILE: u8 = 3;
pub const VIDEO: u8 = 4;
/// Big enough for a key frame of a 1440p screen share.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
/// File chunks stay small so voice keeps flowing during a transfer.
pub const FILE_CHUNK: usize = 32 * 1024;

/// What a connection's writer thread is told to do. One writer per connection
/// means a slow member never holds up anyone else.
pub enum Out {
    Frame(Vec<u8>),
    /// Flush what's queued, then close the connection.
    Close,
}

/// Runs the writer for `stream` until it's told to close or a write fails.
pub fn writer(mut stream: std::net::TcpStream, rx: std::sync::mpsc::Receiver<Out>) {
    std::thread::spawn(move || {
        for out in rx {
            match out {
                Out::Frame(bytes) => {
                    if stream.write_all(&bytes).is_err() {
                        break;
                    }
                }
                Out::Close => break,
            }
        }
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
}

#[cfg(test)]
pub fn write(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(kind);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)
}

pub fn read(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME || !((JSON..=VIDEO).contains(&head[0])) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a KittyConnect room"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

pub fn json(v: &serde_json::Value) -> Vec<u8> {
    let mut out = vec![JSON];
    let body = serde_json::to_vec(v).unwrap_or_default();
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn audio(prefix: &[u8], frame: &[u8]) -> Vec<u8> {
    binary(AUDIO, prefix, frame)
}

pub fn binary(kind: u8, prefix: &[u8], frame: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(&((prefix.len() + frame.len()) as u32).to_be_bytes());
    out.extend_from_slice(prefix);
    out.extend_from_slice(frame);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write(&mut buf, JSON, b"{\"t\":\"hi\"}").unwrap();
        buf.extend(audio(&[0, 1], &[9, 9, 9]));
        let mut r = &buf[..];
        assert_eq!(read(&mut r).unwrap(), (JSON, b"{\"t\":\"hi\"}".to_vec()));
        assert_eq!(read(&mut r).unwrap(), (AUDIO, vec![0, 1, 9, 9, 9]));
    }

    #[test]
    fn rejects_other_protocols() {
        let mut r: &[u8] = b"GET / HTTP/1.1\r\n";
        assert!(read(&mut r).is_err());
    }
}
