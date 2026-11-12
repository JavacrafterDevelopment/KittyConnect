//! Share codes: one line of text that carries everything a peer needs.
//!
//!   kitty:file:<port>:<tailcat address>   file sharing
//!   <tailcat address>                     ports (the port numbers are told separately)
//!
//! Older `kitty:port:<ports>:<address>` codes are still accepted.
//!
//! Plain ASCII, no spaces, so it survives a Discord message. Discord turns
//! `_x_` into italics, which is why "Copy for Discord" wraps the code in
//! backticks, and why parsing strips backticks and backslash escapes.

use crate::tailcat::is_addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Port,
    Voip,
    /// A bare `tc…` address with no KittyConnect wrapper.
    Bare,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Code {
    pub kind: Kind,
    pub ports: Vec<u16>,
    pub addr: String,
}

pub fn file(port: u16, addr: &str) -> String {
    format!("kitty:file:{port}:{addr}")
}

pub fn voip(port: u16, addr: &str) -> String {
    format!("kitty:voip:{port}:{addr}")
}

pub fn parse(input: &str) -> Result<Code, String> {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '`' && *c != '\\' && *c != '*' && *c != '|')
        .collect();
    if cleaned.is_empty() {
        return Err("Paste a code first.".into());
    }

    let Some(start) = cleaned.find("kitty:") else {
        // A bare TailCat address still works; the caller supplies the ports.
        if let Some(addr) = cleaned.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).find(|t| is_addr(t)) {
            return Ok(Code { kind: Kind::Bare, ports: vec![], addr: addr.to_string() });
        }
        return Err("That doesn't look like a KittyConnect code. It should start with kitty:".into());
    };

    let mut parts = cleaned[start + "kitty:".len()..].splitn(3, ':');
    let kind = match parts.next() {
        Some("file") => Kind::File,
        Some("port") => Kind::Port,
        Some("voip") => Kind::Voip,
        _ => return Err("Unknown code type. Expected kitty:file:… or kitty:voip:…".into()),
    };
    let ports = parts
        .next()
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| format!("Bad port number in code: {s}")))
        .collect::<Result<Vec<_>, _>>()?;
    let addr: String = parts
        .next()
        .unwrap_or_default()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if ports.is_empty() {
        return Err("The code has no port in it. Ask for the code again.".into());
    }
    if !is_addr(&addr) {
        return Err("The code is cut off. Ask your friend to copy it again.".into());
    }
    Ok(Code { kind, ports, addr })
}

/// "25565, 8080" → [25565, 8080], no duplicates, in the order given.
pub fn parse_ports(s: &str) -> Result<Vec<u16>, String> {
    let mut out = Vec::new();
    for tok in s.split(|c: char| c == ',' || c == ' ' || c == ';').filter(|t| !t.is_empty()) {
        let p = tok
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| format!("\"{tok}\" isn't a port. Use numbers from 1 to 65535."))?;
        if !out.contains(&p) {
            out.push(p);
        }
    }
    if out.is_empty() {
        return Err("Enter at least one port, like 25565.".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "tcomFwWCCcjS5nKNqAod034nWoJZW0LZqDhhC8U_dKdnDRYQ8uNGFpGQEu";

    #[test]
    fn round_trips() {
        let c = parse(&file(41234, A)).unwrap();
        assert_eq!(c, Code { kind: Kind::File, ports: vec![41234], addr: A.into() });
        let c = parse(&voip(50001, A)).unwrap();
        assert_eq!(c, Code { kind: Kind::Voip, ports: vec![50001], addr: A.into() });
        // Older kitty:port codes still parse.
        let c = parse(&format!("kitty:port:25565,8080:{A}")).unwrap();
        assert_eq!(c.ports, vec![25565, 8080]);
    }

    #[test]
    fn survives_discord() {
        let escaped = format!("hey use this `{}`", file(5000, A).replace('_', "\\_"));
        assert_eq!(parse(&escaped).unwrap().addr, A);
        let wrapped = format!("kitty:port:25565:{}\n{}", &A[..20], &A[20..]);
        assert_eq!(parse(&wrapped).unwrap().addr, A);
    }

    #[test]
    fn bare_address() {
        assert_eq!(parse(A).unwrap().kind, Kind::Bare);
        // Pasted back out of Discord: backticks and escaped underscores.
        let c = parse(&format!("`{}`", A.replace('_', "\\_"))).unwrap();
        assert_eq!((c.kind, c.addr.as_str()), (Kind::Bare, A));
    }

    #[test]
    fn ports_list() {
        assert_eq!(parse_ports("25565, 8080 25565").unwrap(), vec![25565, 8080]);
        assert!(parse_ports("0").is_err());
        assert!(parse_ports("abc").is_err());
    }
}
