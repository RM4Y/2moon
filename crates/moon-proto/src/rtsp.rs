//! Minimal RTSP/1.0 message codec used for stream negotiation, plus SDP
//! attribute helpers for the ANNOUNCE / DESCRIBE bodies.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartLine {
    Request { method: String, target: String },
    Response { code: u16, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub start: StartLine,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    pub fn request(method: &str, target: &str) -> Self {
        Self { start: StartLine::Request { method: method.into(), target: target.into() }, headers: vec![], body: vec![] }
    }

    pub fn response(code: u16, reason: &str) -> Self {
        Self { start: StartLine::Response { code, reason: reason.into() }, headers: vec![], body: vec![] }
    }

    pub fn header(mut self, k: &str, v: impl ToString) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }

    pub fn method(&self) -> Option<&str> {
        match &self.start {
            StartLine::Request { method, .. } => Some(method),
            _ => None,
        }
    }

    pub fn target(&self) -> Option<&str> {
        match &self.start {
            StartLine::Request { target, .. } => Some(target),
            _ => None,
        }
    }

    pub fn code(&self) -> Option<u16> {
        match &self.start {
            StartLine::Response { code, .. } => Some(*code),
            _ => None,
        }
    }

    pub fn cseq(&self) -> Option<u32> {
        self.get("CSeq").and_then(|v| v.trim().parse().ok())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut s = match &self.start {
            StartLine::Request { method, target } => format!("{method} {target} RTSP/1.0\r\n"),
            StartLine::Response { code, reason } => format!("RTSP/1.0 {code} {reason}\r\n"),
        };
        let mut has_len = false;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("Content-Length") {
                has_len = true;
            }
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        if !self.body.is_empty() && !has_len {
            s.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        s.push_str("\r\n");
        let mut out = s.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }

    /// Try to parse one message from `buf`. Returns the message and the number
    /// of bytes consumed, or `Ok(None)` if more data is needed.
    pub fn parse(buf: &[u8]) -> Result<Option<(Self, usize)>, &'static str> {
        let Some(hdr_end) = find(buf, b"\r\n\r\n") else {
            if buf.len() > 64 * 1024 {
                return Err("header too large");
            }
            return Ok(None);
        };
        let head = std::str::from_utf8(&buf[..hdr_end]).map_err(|_| "non-utf8 header")?;
        let mut lines = head.split("\r\n");
        let first = lines.next().ok_or("empty")?;
        let start = if let Some(rest) = first.strip_prefix("RTSP/1.0 ") {
            let (code, reason) = rest.split_once(' ').unwrap_or((rest, ""));
            StartLine::Response { code: code.parse().map_err(|_| "bad status")?, reason: reason.to_string() }
        } else {
            let mut parts = first.splitn(3, ' ');
            let method = parts.next().ok_or("no method")?.to_string();
            let target = parts.next().ok_or("no target")?.to_string();
            StartLine::Request { method, target }
        };
        let mut headers = Vec::new();
        for l in lines {
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let mut m = Self { start, headers, body: vec![] };
        let len: usize = m.get("Content-Length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        let total = hdr_end + 4 + len;
        if buf.len() < total {
            return Ok(None);
        }
        m.body = buf[hdr_end + 4..total].to_vec();
        Ok(Some((m, total)))
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Parse `a=key:value` lines of an SDP body into a map.
pub fn sdp_attributes(body: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=") {
            if let Some((k, v)) = rest.split_once(':') {
                m.insert(k.trim().to_string(), v.trim().to_string());
            } else {
                m.insert(rest.to_string(), String::new());
            }
        }
    }
    m
}

/// Extract `server_port=NNNN` from a Transport header.
pub fn transport_server_port(transport: &str) -> Option<u16> {
    transport.split(';').find_map(|p| p.trim().strip_prefix("server_port=")).and_then(|p| p.split('-').next()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let m = Message::request("ANNOUNCE", "streamid=control/13/0").header("CSeq", 6).with_body(b"a=x:1\r\n".to_vec());
        let enc = m.encode();
        let (p, n) = Message::parse(&enc).unwrap().unwrap();
        assert_eq!(n, enc.len());
        assert_eq!(p.method(), Some("ANNOUNCE"));
        assert_eq!(p.cseq(), Some(6));
        assert_eq!(p.body, b"a=x:1\r\n");
        assert!(Message::parse(&enc[..enc.len() - 1]).unwrap().is_none());
        let r = Message::response(200, "OK").header("Transport", "server_port=47998");
        let (p, _) = Message::parse(&r.encode()).unwrap().unwrap();
        assert_eq!(p.code(), Some(200));
        assert_eq!(transport_server_port(p.get("transport").unwrap()), Some(47998));
    }

    #[test]
    fn sdp() {
        let a = sdp_attributes("v=0\r\na=x-nv-video[0].clientViewportWd:1920 \r\na=flag\r\n");
        assert_eq!(a["x-nv-video[0].clientViewportWd"], "1920");
        assert!(a.contains_key("flag"));
    }
}
