//! Parser properties (TEST-PLAN SEC-23, U2): arbitrary input never panics, is
//! either accepted or refused, and anything accepted survives canonicalization.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use proptest::prelude::*;

use super::docker::CREATE;
use super::http::{BodyState, Conn, HttpError, HttpLimits, parse_request_head, parse_target};
use super::json::{Json, parse};
use super::schema::check;

fn no_dups(j: &Json) -> bool {
    match j {
        Json::Obj(m) => {
            let mut seen = std::collections::HashSet::new();
            m.iter().all(|(k, v)| seen.insert(k.clone()) && no_dups(v))
        }
        Json::Arr(a) => a.iter().all(no_dups),
        _ => true,
    }
}

fn small_limits() -> HttpLimits {
    HttpLimits {
        max_head: 2048,
        max_fields: 20,
        max_json: 4096,
        head_deadline: Duration::from_millis(400),
        idle: Duration::from_millis(400),
        progress: Duration::from_millis(200),
    }
}

/// Feed `data` to a Conn in pieces of `step` bytes and read one request and its
/// body the way the endpoint does.
fn drive(data: Vec<u8>, step: usize) -> Result<(String, Vec<u8>), String> {
    let (a, mut b) = UnixStream::pair().unwrap();
    let t = std::thread::spawn(move || {
        for c in data.chunks(step.max(1)) {
            if b.write_all(c).is_err() {
                return;
            }
            let _ = b.flush();
        }
        // Drop closes the write side: truncated input must be a clean error.
    });
    let mut conn = Conn::new(a, small_limits());
    let r = match conn.read_request_head() {
        Ok(h) => {
            let target = h.target.clone();
            match conn.read_body_limited(h.framing, 4096) {
                Ok(body) => Ok((target, body)),
                Err(e) => Err(format!("{e:?}")),
            }
        }
        Err(e) => Err(format!("{e:?}")),
    };
    drop(conn);
    let _ = t.join();
    r
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("E3_FUZZ_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(400), ..ProptestConfig::default() })]

    #[test]
    fn json_parse_is_total_and_canonical(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        if let Ok(v) = parse(&bytes) {
            let again = parse(&v.to_bytes()).expect("canonical form parses");
            prop_assert_eq!(again, v);
        }
    }

    #[test]
    fn json_parse_of_json_like_text(s in r#"[\[\]\{\}",:0-9a-zA-Z\\ \n.eE+-]{0,200}"#) {
        if let Ok(v) = parse(s.as_bytes()) {
            prop_assert!(parse(&v.to_bytes()).is_ok());
            prop_assert!(no_dups(&v));
        }
    }

    #[test]
    fn schema_is_total(s in r#"[\[\]\{\}",:0-9a-zA-Z ]{0,160}"#) {
        if let Ok(v) = parse(s.as_bytes()) {
            let _ = check(&CREATE, &v, "$");
        }
    }

    #[test]
    fn target_parse_is_total(s in "\\PC{0,80}") {
        let _ = parse_target(&s);
        let _ = parse_target(&format!("/{s}"));
    }

    #[test]
    fn head_parse_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        let mut raw = bytes;
        raw.extend_from_slice(b"\r\n\r\n");
        let _ = parse_request_head(&raw, &HttpLimits::default());
    }

    #[test]
    fn mutated_requests_are_accepted_or_refused(
        pos in 0usize..160, byte in any::<u8>(), step in 1usize..40, chunked in any::<bool>(),
    ) {
        let body = br#"{"Image":"busybox"}"#;
        let mut req = if chunked {
            let mut r = b"POST /containers/create HTTP/1.1\r\nHost: d\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            r.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
            r.extend_from_slice(body);
            r.extend_from_slice(b"\r\n0\r\n\r\n");
            r
        } else {
            let mut r = format!("POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            r.extend_from_slice(body);
            r
        };
        let p = pos % req.len();
        req[p] = byte;
        // No panic, no hang, and a definite answer.
        let _ = drive(req, step);
    }
}

#[test]
fn every_byte_boundary_gives_the_same_request() {
    let reqs: Vec<Vec<u8>> = vec![
        b"GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n".to_vec(),
        b"POST /v1.47/containers/create?name=x HTTP/1.1\r\nHost: d\r\nContent-Length: 19\r\nContent-Type: application/json\r\n\r\n{\"Image\":\"busybox\"}".to_vec(),
        b"POST /v1.47/containers/create HTTP/1.1\r\nHost: d\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"Ima\r\n0e\r\nge\":\"busybox\"}\r\n0\r\n\r\n".to_vec(),
    ];
    for r in reqs {
        let whole = drive(r.clone(), r.len()).unwrap();
        for step in [1, 2, 3, 7] {
            assert_eq!(drive(r.clone(), step).unwrap(), whole, "step {step}");
        }
    }
}

#[test]
fn smuggling_shapes_are_refused_by_the_reader() {
    let create = br#"{"HostConfig":{"Privileged":true}}"#;
    let mut bare_lf = b"GET /_ping HTTP/1.1\nHost:d\n\nPOST /containers/create HTTP/1.1\nHost:d\nContent-Length: 34\n\n".to_vec();
    bare_lf.extend_from_slice(create);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("bare LF head", bare_lf),
        (
            "CL and TE",
            b"POST /x HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
                .to_vec(),
        ),
        (
            "chunk size sign",
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n+5\r\nhello\r\n0\r\n\r\n"
                .to_vec(),
        ),
        (
            "chunk extension",
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5;x=y\r\nhello\r\n0\r\n\r\n"
                .to_vec(),
        ),
        (
            "chunk size overflow",
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nfffffffffffffffff\r\n".to_vec(),
        ),
        (
            "trailers",
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX: y\r\n\r\n".to_vec(),
        ),
        (
            "chunk without CRLF",
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhelloXX0\r\n\r\n".to_vec(),
        ),
        (
            "nul in head",
            b"GET /x HTTP/1.1\r\nA: b\0c\r\n\r\n".to_vec(),
        ),
        (
            "huge header",
            [
                b"GET /x HTTP/1.1\r\nA: ".as_slice(),
                &vec![b'a'; 5000],
                b"\r\n\r\n",
            ]
            .concat(),
        ),
        (
            "truncated body",
            b"POST /x HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc".to_vec(),
        ),
    ];
    for (name, c) in cases {
        let r = drive(c, 64);
        assert!(r.is_err(), "{name}: {r:?}");
    }
    let _ = BodyState::Done;
    let _ = HttpError::Closed;
}

#[test]
fn deadlines_end_stalled_requests() {
    // A head that never completes, and a body that stalls: both end within
    // the (shortened) progress deadlines instead of holding the connection.
    for (name, prefix) in [
        ("stalled head", b"POST /containers/create HTTP/1.1\r\nHost: d\r\n".to_vec()),
        ("stalled body", b"POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Length: 100\r\n\r\n{\"Image\":".to_vec()),
    ] {
        let (a, mut b) = UnixStream::pair().unwrap();
        b.write_all(&prefix).unwrap();
        let mut conn = Conn::new(a, small_limits());
        let t0 = std::time::Instant::now();
        let r = conn.read_request_head().and_then(|h| conn.read_body_limited(h.framing, 4096).map(|_| h));
        assert!(matches!(r, Err(HttpError::Timeout | HttpError::Bad(_))), "{name}: {r:?}");
        assert!(t0.elapsed() < Duration::from_secs(3), "{name} took {:?}", t0.elapsed());
        drop(b);
    }
}

#[test]
fn exact_limits() {
    let l = small_limits();
    let line = "GET /x HTTP/1.1\r\n";
    // Head of exactly max_head bytes is accepted, one more is not.
    let pad = |total: usize| {
        let mut h = format!("{line}A: ");
        let fill = total - h.len() - 4;
        h.push_str(&"a".repeat(fill));
        h.push_str("\r\n\r\n");
        assert_eq!(h.len(), total);
        h
    };
    assert!(parse_request_head(pad(l.max_head).as_bytes(), &l).is_ok());
    let r = drive(pad(l.max_head + 1).into_bytes(), 512);
    assert!(
        matches!(r, Err(ref e) if e.contains("HeadersTooLarge")),
        "{r:?}"
    );
    // Field count.
    let fields = |n: usize| {
        let mut h = String::from(line);
        for i in 0..n {
            h.push_str(&format!("H{i}: v\r\n"));
        }
        h.push_str("\r\n");
        h
    };
    assert!(parse_request_head(fields(l.max_fields).as_bytes(), &l).is_ok());
    assert!(matches!(
        parse_request_head(fields(l.max_fields + 1).as_bytes(), &l),
        Err(HttpError::HeadersTooLarge)
    ));
    // JSON body limit.
    let body = |n: usize| {
        let mut b = vec![b'x'; n];
        b[0] = b'[';
        b[n - 1] = b']';
        b
    };
    for (n, ok) in [(4096usize, true), (4097, false)] {
        let mut req = format!("POST /x HTTP/1.1\r\nContent-Length: {n}\r\n\r\n").into_bytes();
        req.extend_from_slice(&body(n));
        let r = drive(req, 1024);
        assert_eq!(r.is_ok(), ok, "{n}: {r:?}");
        if !ok {
            assert!(r.unwrap_err().contains("BodyTooLarge"));
        }
    }
    // Chunked bodies are bounded as they stream, never trusted by size line.
    let mut req = b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..10 {
        req.extend_from_slice(b"1000\r\n");
        req.extend_from_slice(&vec![b'a'; 0x1000]);
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(b"0\r\n\r\n");
    assert!(drive(req, 4096).unwrap_err().contains("BodyTooLarge"));
}
