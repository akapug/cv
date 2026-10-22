//! Raw HTTP for integration tests, including deliberately malformed custom headers.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Send one HTTP/1.1 request and read until the server closes the connection.
/// Supply Host and Content-Length only when the caller has not provided them;
/// custom duplicate headers are preserved for framing tests.
pub fn raw_request(addr: &str, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to test server");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    if !headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("host")) {
        request.push_str(&format!("Host: {addr}\r\n"));
    }
    if let Some(body) = body.filter(|_| {
        !headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("content-length"))
    }) {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (key, value) in headers {
        request.push_str(&format!("{key}: {value}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    request.push_str(body.unwrap_or(""));
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read test response");
    String::from_utf8(response).expect("test response is UTF-8")
}

/// Require a complete response header before interpreting its body.
pub fn parts(raw: &str) -> (&str, &str) {
    raw.split_once("\r\n\r\n").expect("HTTP response header terminator")
}

pub fn status(raw: &str) -> u16 {
    raw.lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("bad status line in: {raw}"))
}
