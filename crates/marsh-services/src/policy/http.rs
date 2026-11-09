//! Strict HTTP/1.x framing (B1): requests are parsed, never forwarded.
//!
//! The parser accepts only what the rebuild step can re-serialize without
//! ambiguity: CRLF line ends, token header names, no obs-fold, exactly one
//! framing, an origin-form target. Everything else is a 400 and a close.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct HttpLimits {
    pub max_head: usize,
    pub max_fields: usize,
    pub max_json: usize,
    /// Whole request head must arrive within this (slowloris).
    pub head_deadline: Duration,
    /// Between requests on a kept-alive connection.
    pub idle: Duration,
    /// Any single read inside a body or head may stall this long.
    pub progress: Duration,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_head: 32 * 1024,
            max_fields: 100,
            max_json: 1024 * 1024,
            head_deadline: Duration::from_secs(60),
            idle: Duration::from_secs(120),
            progress: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
pub enum HttpError {
    /// Clean EOF between requests.
    Closed,
    /// 400 and close.
    Bad(&'static str),
    /// 431 and close.
    HeadersTooLarge,
    /// 413 and close.
    BodyTooLarge,
    /// `Expect:` present: 417 and close.
    Expect,
    Timeout,
    Io(io::Error),
}

impl From<io::Error> for HttpError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => HttpError::Timeout,
            _ => HttpError::Io(e),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    None,
    Length(u64),
    Chunked,
}

#[derive(Debug, Clone)]
pub struct Head {
    pub method: String,
    pub target: String,
    pub minor: u8,
    /// Lowercased names, trimmed values, in order.
    pub headers: Vec<(String, String)>,
    pub framing: Framing,
    pub close: bool,
    pub upgrade: Option<String>,
    pub connection_upgrade: bool,
    pub h2c_settings: bool,
    /// The head bytes as received. Only the break modes read them.
    pub raw: Vec<u8>,
}

impl Head {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn headers_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.headers
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct RespHead {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub framing: RespFraming,
    pub content_type: Option<String>,
    pub upgrade: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespFraming {
    NoBody,
    Length(u64),
    Chunked,
    UntilClose,
}

/// Body decoder state for one message.
#[derive(Debug, Clone, Copy)]
pub enum BodyState {
    Done,
    Length(u64),
    /// Bytes left in the current chunk; `None` before the first size line.
    Chunked(Option<u64>),
}

impl BodyState {
    #[must_use]
    pub fn new(f: Framing) -> Self {
        match f {
            Framing::None | Framing::Length(0) => BodyState::Done,
            Framing::Length(n) => BodyState::Length(n),
            Framing::Chunked => BodyState::Chunked(None),
        }
    }
}

/// Called when a read has been quiet for `interval`; `true` aborts the read.
pub type Poll = (Duration, Box<dyn FnMut() -> bool + Send>);

pub struct Conn {
    pub stream: UnixStream,
    buf: Vec<u8>,
    pos: usize,
    pub limits: HttpLimits,
    pub poll: Option<Poll>,
}

fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

impl Conn {
    #[must_use]
    pub fn new(stream: UnixStream, limits: HttpLimits) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            pos: 0,
            limits,
            poll: None,
        }
    }

    fn avail(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos > 64 * 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }

    /// Bytes read from the client but not yet consumed.
    pub fn take_buffered(&mut self) -> Vec<u8> {
        let v = self.buf[self.pos..].to_vec();
        self.buf.clear();
        self.pos = 0;
        v
    }

    fn fill(&mut self, wait: Duration) -> Result<usize, HttpError> {
        let deadline = Instant::now() + wait;
        let mut tmp = [0u8; 16 * 1024];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let slice = match &self.poll {
                Some((iv, _)) => left.min(*iv),
                None => left,
            };
            // macOS refuses setsockopt once the peer has closed; the read below
            // then reports the data or the EOF.
            let _ = self
                .stream
                .set_read_timeout(Some(slice.max(Duration::from_millis(1))));
            match self.stream.read(&mut tmp) {
                Ok(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                    return Ok(n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if let Some((_, f)) = &mut self.poll
                        && f()
                    {
                        return Err(HttpError::Closed);
                    }
                    if Instant::now() >= deadline {
                        return Err(HttpError::Timeout);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Bytes until the peer closes (close-delimited response bodies, raw
    /// streams). 0 at EOF.
    pub fn read_until_close(&mut self, out: &mut [u8]) -> Result<usize, HttpError> {
        self.read_raw(out)
    }

    /// Read one request head. `Err(Closed)` on EOF before any byte.
    pub fn read_request_head(&mut self) -> Result<Head, HttpError> {
        let raw = self.read_head_bytes(true)?;
        parse_request_head(&raw, &self.limits)
    }

    /// Returns the head bytes including the final CRLFCRLF.
    fn read_head_bytes(&mut self, request: bool) -> Result<Vec<u8>, HttpError> {
        let mut deadline: Option<Instant> = None;
        let mut scanned = 0usize;
        loop {
            let a = self.avail();
            // Validate newly arrived bytes: CRLF only, no control bytes. Only
            // the head is checked; whatever follows it is somebody's body.
            let end = find_crlfcrlf(a).map_or(a.len(), |e| e + 4);
            for i in scanned.min(end)..end {
                let c = a[i];
                if c == b'\n' && (i == 0 || a[i - 1] != b'\r') {
                    return Err(HttpError::Bad("bare LF in request head"));
                }
                if c == b'\r' && a.get(i + 1).is_some_and(|n| *n != b'\n') {
                    return Err(HttpError::Bad("bare CR in request head"));
                }
                if (c < 0x20 && c != b'\t' && c != b'\r' && c != b'\n') || c == 0x7f {
                    return Err(HttpError::Bad("control byte in request head"));
                }
            }
            scanned = end.saturating_sub(1);
            if let Some(end) = find_crlfcrlf(a) {
                if end + 4 > self.limits.max_head {
                    return Err(HttpError::HeadersTooLarge);
                }
                let head = a[..end + 4].to_vec();
                self.consume(end + 4);
                return Ok(head);
            }
            if a.len() > self.limits.max_head {
                return Err(HttpError::HeadersTooLarge);
            }
            let wait = if a.is_empty() && request {
                self.limits.idle
            } else {
                let d = *deadline.get_or_insert_with(|| Instant::now() + self.limits.head_deadline);
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(HttpError::Timeout);
                }
                left.min(self.limits.progress)
            };
            let was_empty = a.is_empty();
            match self.fill(wait) {
                Ok(0) => {
                    return if was_empty && request {
                        Err(HttpError::Closed)
                    } else {
                        Err(HttpError::Bad("connection closed inside the request head"))
                    };
                }
                Ok(_) => {}
                Err(HttpError::Timeout) if was_empty && request => return Err(HttpError::Closed),
                Err(e) => return Err(e),
            }
        }
    }

    /// Next decoded body bytes into `out`; 0 means the message body ended.
    pub fn read_body(&mut self, st: &mut BodyState, out: &mut [u8]) -> Result<usize, HttpError> {
        loop {
            match st {
                BodyState::Done => return Ok(0),
                BodyState::Length(left) => {
                    let want = usize::try_from((*left).min(out.len() as u64)).unwrap_or(out.len());
                    let n = self.read_raw(&mut out[..want])?;
                    if n == 0 {
                        return Err(HttpError::Bad("connection closed inside the request body"));
                    }
                    *left -= n as u64;
                    if *left == 0 {
                        *st = BodyState::Done;
                    }
                    return Ok(n);
                }
                BodyState::Chunked(None) => {
                    let size = self.read_chunk_size()?;
                    if size == 0 {
                        // No trailers: the next two bytes must be CRLF.
                        self.need(2)?;
                        if &self.avail()[..2] != b"\r\n" {
                            return Err(HttpError::Bad("chunked trailers are not accepted"));
                        }
                        self.consume(2);
                        *st = BodyState::Done;
                    } else {
                        *st = BodyState::Chunked(Some(size));
                    }
                }
                BodyState::Chunked(Some(left)) => {
                    let want = usize::try_from((*left).min(out.len() as u64)).unwrap_or(out.len());
                    let n = self.read_raw(&mut out[..want])?;
                    if n == 0 {
                        return Err(HttpError::Bad("connection closed inside a chunk"));
                    }
                    *left -= n as u64;
                    if *left == 0 {
                        self.need(2)?;
                        if &self.avail()[..2] != b"\r\n" {
                            return Err(HttpError::Bad("chunk is not terminated by CRLF"));
                        }
                        self.consume(2);
                        *st = BodyState::Chunked(None);
                    }
                    return Ok(n);
                }
            }
        }
    }

    fn need(&mut self, n: usize) -> Result<(), HttpError> {
        while self.avail().len() < n {
            if self.fill(self.limits.progress)? == 0 {
                return Err(HttpError::Bad("connection closed inside the request body"));
            }
        }
        Ok(())
    }

    fn read_chunk_size(&mut self) -> Result<u64, HttpError> {
        loop {
            let a = self.avail();
            if let Some(i) = a.iter().position(|b| *b == b'\n') {
                let line = &a[..i];
                if i == 0 || line[i - 1] != b'\r' {
                    return Err(HttpError::Bad("bad chunk size line"));
                }
                let digits = &line[..i - 1];
                if digits.is_empty()
                    || digits.len() > 16
                    || !digits.iter().all(u8::is_ascii_hexdigit)
                {
                    return Err(HttpError::Bad("bad chunk size"));
                }
                let v = u64::from_str_radix(std::str::from_utf8(digits).unwrap_or("x"), 16)
                    .map_err(|_| HttpError::Bad("bad chunk size"))?;
                self.consume(i + 1);
                return Ok(v);
            }
            if a.len() > 18 {
                return Err(HttpError::Bad("chunk size line too long"));
            }
            if self.fill(self.limits.progress)? == 0 {
                return Err(HttpError::Bad("connection closed inside the request body"));
            }
        }
    }

    fn read_raw(&mut self, out: &mut [u8]) -> Result<usize, HttpError> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.avail().is_empty() && self.fill(self.limits.progress)? == 0 {
            return Ok(0);
        }
        let a = self.avail();
        let n = a.len().min(out.len());
        out[..n].copy_from_slice(&a[..n]);
        self.consume(n);
        Ok(n)
    }

    /// Read a whole body, bounded. Used for JSON.
    pub fn read_body_limited(
        &mut self,
        framing: Framing,
        limit: usize,
    ) -> Result<Vec<u8>, HttpError> {
        if let Framing::Length(n) = framing
            && n > limit as u64
        {
            return Err(HttpError::BodyTooLarge);
        }
        let mut st = BodyState::new(framing);
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = self.read_body(&mut st, &mut chunk)?;
            if n == 0 {
                return Ok(out);
            }
            if out.len() + n > limit {
                return Err(HttpError::BodyTooLarge);
            }
            out.extend_from_slice(&chunk[..n]);
        }
    }

    /// Discard the rest of a body if it is small; otherwise report that the
    /// stream cannot be kept in sync.
    pub fn drain(&mut self, framing: Framing, max: u64) -> bool {
        if let Framing::Length(n) = framing
            && n > max
        {
            return false;
        }
        let mut st = BodyState::new(framing);
        let mut total = 0u64;
        let mut chunk = [0u8; 8192];
        loop {
            match self.read_body(&mut st, &mut chunk) {
                Ok(0) => return true,
                Ok(n) => {
                    total += n as u64;
                    if total > max {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// Response head from the upstream engine (a trusted Go server, but still
    /// bounded and parsed strictly enough to know where the body ends).
    pub fn read_response_head(&mut self, head_request: bool) -> Result<RespHead, HttpError> {
        let raw = self.read_head_bytes(false)?;
        parse_response_head(&raw, head_request)
    }
}

fn find_crlfcrlf(a: &[u8]) -> Option<usize> {
    a.windows(4).position(|w| w == b"\r\n\r\n")
}

fn split_lines(raw: &[u8]) -> Vec<&[u8]> {
    // raw ends with CRLFCRLF; the last two "lines" are empty.
    let body = &raw[..raw.len().saturating_sub(4)];
    body.split(|b| *b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .collect()
}

fn parse_fields(lines: &[&[u8]], limits: &HttpLimits) -> Result<Vec<(String, String)>, HttpError> {
    if lines.len() > limits.max_fields {
        return Err(HttpError::HeadersTooLarge);
    }
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        if l.is_empty() {
            return Err(HttpError::Bad("empty header line"));
        }
        if l[0] == b' ' || l[0] == b'\t' {
            return Err(HttpError::Bad("obs-fold header lines are not accepted"));
        }
        let colon = l
            .iter()
            .position(|b| *b == b':')
            .ok_or(HttpError::Bad("header line without a colon"))?;
        let name = &l[..colon];
        if name.is_empty() || !name.iter().all(|b| is_tchar(*b)) {
            return Err(HttpError::Bad("invalid header name"));
        }
        let value = trim_ows(&l[colon + 1..]);
        out.push((
            String::from_utf8_lossy(name).to_ascii_lowercase(),
            // Values that are not ASCII are kept lossily; only allowlisted
            // headers are ever forwarded and those are re-validated.
            String::from_utf8_lossy(value).into_owned(),
        ));
    }
    Ok(out)
}

fn trim_ows(mut s: &[u8]) -> &[u8] {
    while let Some((f, r)) = s.split_first() {
        if *f == b' ' || *f == b'\t' {
            s = r;
        } else {
            break;
        }
    }
    while let Some((l, r)) = s.split_last() {
        if *l == b' ' || *l == b'\t' {
            s = r;
        } else {
            break;
        }
    }
    s
}

pub fn parse_request_head(raw: &[u8], limits: &HttpLimits) -> Result<Head, HttpError> {
    let lines = split_lines(raw);
    let (first, rest) = lines.split_first().ok_or(HttpError::Bad("empty request"))?;
    let line =
        std::str::from_utf8(first).map_err(|_| HttpError::Bad("request line is not ASCII"))?;
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts
        .next()
        .ok_or(HttpError::Bad("malformed request line"))?;
    let version = parts
        .next()
        .ok_or(HttpError::Bad("malformed request line"))?;
    if parts.next().is_some() {
        return Err(HttpError::Bad("malformed request line"));
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(HttpError::Bad("invalid method"));
    }
    let minor = match version {
        "HTTP/1.1" => 1,
        "HTTP/1.0" => 0,
        _ => return Err(HttpError::Bad("only HTTP/1.0 and HTTP/1.1 are accepted")),
    };
    if !target.starts_with('/') {
        return Err(HttpError::Bad(
            "only origin-form request targets are accepted",
        ));
    }
    if !target.bytes().all(|b| (0x21..0x7f).contains(&b)) || target.contains('#') {
        return Err(HttpError::Bad("invalid byte in the request target"));
    }
    let headers = parse_fields(rest, limits)?;

    let mut cl: Option<u64> = None;
    let mut te = false;
    let mut close = minor == 0;
    let mut upgrade: Option<String> = None;
    let mut connection_upgrade = false;
    let mut h2c_settings = false;
    let mut seen_cl = 0;
    let mut seen_te = 0;
    let mut seen_up = 0;
    for (k, v) in &headers {
        match k.as_str() {
            "content-length" => {
                seen_cl += 1;
                if v.is_empty() || v.len() > 19 || !v.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(HttpError::Bad("invalid Content-Length"));
                }
                cl = Some(
                    v.parse()
                        .map_err(|_| HttpError::Bad("invalid Content-Length"))?,
                );
            }
            "transfer-encoding" => {
                seen_te += 1;
                if !v.eq_ignore_ascii_case("chunked") {
                    return Err(HttpError::Bad("unsupported Transfer-Encoding"));
                }
                te = true;
            }
            "expect" => return Err(HttpError::Expect),
            "upgrade" => {
                seen_up += 1;
                upgrade = Some(v.to_ascii_lowercase());
            }
            "http2-settings" => h2c_settings = true,
            "connection" => {
                for tok in v.split(',') {
                    match tok.trim().to_ascii_lowercase().as_str() {
                        "close" => close = true,
                        "keep-alive" => {
                            if minor == 0 {
                                close = false;
                            }
                        }
                        "upgrade" => connection_upgrade = true,
                        "http2-settings" => h2c_settings = true,
                        _ => return Err(HttpError::Bad("unsupported Connection token")),
                    }
                }
            }
            _ => {}
        }
    }
    if seen_cl > 1 || seen_te > 1 || seen_up > 1 {
        return Err(HttpError::Bad("duplicate framing or upgrade header"));
    }
    if cl.is_some() && te {
        return Err(HttpError::Bad("both Content-Length and Transfer-Encoding"));
    }
    if te && minor == 0 {
        return Err(HttpError::Bad("Transfer-Encoding in an HTTP/1.0 request"));
    }
    let framing = match (cl, te) {
        (_, true) => Framing::Chunked,
        (Some(n), _) => Framing::Length(n),
        _ => Framing::None,
    };
    Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        minor,
        headers,
        framing,
        close,
        upgrade,
        connection_upgrade,
        h2c_settings,
        raw: raw.to_vec(),
    })
}

fn parse_response_head(raw: &[u8], head_request: bool) -> Result<RespHead, HttpError> {
    let limits = HttpLimits {
        max_fields: 400,
        ..HttpLimits::default()
    };
    let lines = split_lines(raw);
    let (first, rest) = lines
        .split_first()
        .ok_or(HttpError::Bad("empty response"))?;
    let line =
        std::str::from_utf8(first).map_err(|_| HttpError::Bad("response line is not ASCII"))?;
    let rest_line = line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| line.strip_prefix("HTTP/1.0 "))
        .ok_or(HttpError::Bad("bad response line"))?;
    let (code, reason) = rest_line.split_at(rest_line.find(' ').unwrap_or(rest_line.len()));
    let status: u16 = code
        .parse()
        .map_err(|_| HttpError::Bad("bad response status"))?;
    if !(100..=599).contains(&status) {
        return Err(HttpError::Bad("bad response status"));
    }
    let headers = parse_fields(rest, &limits)?;
    let mut cl = None;
    let mut chunked = false;
    let mut content_type = None;
    let mut upgrade = None;
    for (k, v) in &headers {
        match k.as_str() {
            "content-length" => cl = v.parse::<u64>().ok(),
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            "content-type" => content_type = Some(v.clone()),
            "upgrade" => upgrade = Some(v.to_ascii_lowercase()),
            _ => {}
        }
    }
    let framing = if head_request || status == 204 || status == 304 || (100..200).contains(&status)
    {
        RespFraming::NoBody
    } else if chunked {
        RespFraming::Chunked
    } else if let Some(n) = cl {
        RespFraming::Length(n)
    } else {
        RespFraming::UntilClose
    };
    let reason: String = reason
        .trim()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(64)
        .collect();
    Ok(RespHead {
        status,
        reason,
        headers,
        framing,
        content_type,
        upgrade,
    })
}

#[must_use]
pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        417 => "Expectation Failed",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// A local refusal. `{"message":"marsh: <reason>"}`.
pub fn write_refusal<W: Write>(
    w: &mut W,
    status: u16,
    reason: &str,
    close: bool,
) -> io::Result<()> {
    let msg = reason
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    let body = format!(
        "{{\"message\":{}}}\n",
        crate::policy::json::Json::Str(format!("marsh: {msg}")).to_string_json()
    );
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n",
        status_text(status),
        body.len(),
        if close { "Connection: close\r\n" } else { "" }
    );
    w.write_all(head.as_bytes())?;
    w.write_all(body.as_bytes())?;
    w.flush()
}

impl crate::policy::json::Json {
    #[must_use]
    pub fn to_string_json(&self) -> String {
        String::from_utf8_lossy(&self.to_bytes()).into_owned()
    }
}

// --- request target -------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub segments: Vec<String>,
    pub query: Vec<(String, String)>,
}

fn hexval(b: u8) -> Option<u8> {
    (b as char).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

fn pct_decode(s: &str, plus_is_space: bool) -> Result<Vec<u8>, &'static str> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = b.get(i + 1).copied().and_then(hexval);
                let l = b.get(i + 2).copied().and_then(hexval);
                match (h, l) {
                    (Some(h), Some(l)) => out.push(h * 16 + l),
                    _ => return Err("invalid percent-escape"),
                }
                i += 3;
                continue;
            }
            b'+' if plus_is_space => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    Ok(out)
}

fn path_char_ok(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-._~:@+=,".contains(&c)
}

/// Refuse anything Docker's router could read differently from us (any
/// percent-escape in the path, `//`, dot segments, non-ASCII). Query values are
/// decoded once. Errors are policy refusals (403).
pub fn parse_target(raw: &str) -> Result<Target, &'static str> {
    let (path, query) = match raw.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (raw, None),
    };
    if raw.len() > 8192 {
        return Err("request target is too long");
    }
    let rest = path
        .strip_prefix('/')
        .ok_or("request target must start with /")?;
    let mut segments = Vec::new();
    if !rest.is_empty() {
        for seg in rest.split('/') {
            if seg.is_empty() {
                return Err("empty path segment (// or trailing /) is not accepted");
            }
            // Docker's clients never percent-encode a path, and Go's router
            // decodes before matching, so any escape is a differential risk.
            if !seg.bytes().all(path_char_ok) {
                return Err("path contains an encoded or unsupported character");
            }
            if seg == "." || seg == ".." {
                return Err("dot segments are not accepted");
            }
            segments.push(seg.to_string());
        }
    }
    let mut q = Vec::new();
    if let Some(query) = query
        && !query.is_empty()
    {
        for pair in query.split('&') {
            if pair.is_empty() {
                return Err("empty query parameter");
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = String::from_utf8(pct_decode(k, true)?).map_err(|_| "query is not UTF-8")?;
            let v = String::from_utf8(pct_decode(v, true)?).map_err(|_| "query is not UTF-8")?;
            if k.is_empty() || k.chars().chain(v.chars()).any(char::is_control) {
                return Err("control character in the query");
            }
            if q.iter().any(|(ek, _): &(String, String)| *ek == k) {
                return Err("duplicate query parameter");
            }
            q.push((k, v));
        }
    }
    Ok(Target { segments, query: q })
}

#[must_use]
pub fn encode_query(q: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in q.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        pct_encode(k, &mut out);
        out.push('=');
        pct_encode(v, &mut out);
    }
    out
}

fn pct_encode(s: &str, out: &mut String) {
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(raw: &str) -> Result<Head, HttpError> {
        parse_request_head(raw.as_bytes(), &HttpLimits::default())
    }

    #[test]
    fn head_table() {
        let bad = |raw: &str| matches!(head(raw), Err(HttpError::Bad(_)));
        let ok = "GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n";
        assert!(head(ok).is_ok());
        for (name, raw) in [
            (
                "CL and TE",
                "POST /x HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            ),
            (
                "two CL",
                "POST /x HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
            ),
            (
                "TE chunked, identity",
                "POST /x HTTP/1.1\r\nTransfer-Encoding: chunked, identity\r\n\r\n",
            ),
            (
                "xchunked",
                "POST /x HTTP/1.1\r\nTransfer-Encoding: xchunked\r\n\r\n",
            ),
            (
                "gzip chunked",
                "POST /x HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            ),
            ("obs-fold", "GET /x HTTP/1.1\r\nA: b\r\n c\r\n\r\n"),
            ("space before colon", "GET /x HTTP/1.1\r\nA : b\r\n\r\n"),
            (
                "plus in CL",
                "POST /x HTTP/1.1\r\nContent-Length: +1\r\n\r\n",
            ),
            ("lowercase method", "get /x HTTP/1.1\r\n\r\n"),
            ("h2 preface", "PRI * HTTP/2.0\r\n\r\n"),
            (
                "absolute target",
                "GET http://x/containers/json HTTP/1.1\r\n\r\n",
            ),
            ("two spaces", "GET  /x HTTP/1.1\r\n\r\n"),
            ("http 2.0", "GET /x HTTP/2.0\r\n\r\n"),
            (
                "unknown connection token",
                "GET /x HTTP/1.1\r\nConnection: foo\r\n\r\n",
            ),
            (
                "te in 1.0",
                "POST /x HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
            ),
            (
                "duplicate upgrade",
                "GET /x HTTP/1.1\r\nUpgrade: a\r\nUpgrade: b\r\n\r\n",
            ),
        ] {
            assert!(bad(raw), "{name} must be a 400");
        }
        assert!(matches!(
            head("GET /x HTTP/1.1\r\nExpect: 100-continue\r\n\r\n"),
            Err(HttpError::Expect)
        ));
        let mut many = String::from("GET /x HTTP/1.1\r\n");
        for i in 0..101 {
            many.push_str(&format!("H{i}: v\r\n"));
        }
        many.push_str("\r\n");
        assert!(matches!(head(&many), Err(HttpError::HeadersTooLarge)));
        assert!(head("GET /x HTTP/1.1\r\nContent-Length: 5\r\n\r\n").is_ok());
        assert!(head("GET /x HTTP/1.0\r\n\r\n").unwrap().close);
    }

    #[test]
    fn target_table() {
        for good in [
            "/_ping",
            "/v1.47/containers/json?all=1&filters=%7B%7D",
            "/containers/abc/json",
        ] {
            assert!(parse_target(good).is_ok(), "{good}");
        }
        for bad in [
            "/v1.47/containers/%63reate",
            "/v1.47/containers/%2e%2e",
            "/v1.47/containers%2Fcreate",
            "//containers/create",
            "/a//b",
            "/a/./b",
            "/a/../b",
            "/a/b/",
            "/a/%00",
            "/a/%zz",
            "/a/\u{e9}",
            "/a b",
            "/a?x=1&x=2",
            "/a?&",
        ] {
            assert!(parse_target(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_target("/v1.47/containers/json?filters=%7B%22a%22%3A1%7D&x=a+b")
                .unwrap()
                .query[1]
                .1,
            "a b"
        );
    }
}
