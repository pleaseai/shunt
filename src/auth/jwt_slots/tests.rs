use axum::http::HeaderValue;

use super::*;

fn cookies(fields: &[&[u8]]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for field in fields {
        headers.append(header::COOKIE, HeaderValue::from_bytes(field).unwrap());
    }
    headers
}

fn read<'a>(headers: &'a HeaderMap) -> Presented<'a> {
    cookie_credential(headers, "session")
}

#[test]
fn an_absent_cookie_is_absent() {
    assert_eq!(read(&HeaderMap::new()), Presented::Absent);
    assert_eq!(read(&cookies(&[b"other=1; more=2"])), Presented::Absent);
}

#[test]
fn one_cookie_is_read_among_others_and_across_fields() {
    assert_eq!(
        read(&cookies(&[b"a=1; session=tok; b=2"])),
        Presented::One("tok")
    );
    assert_eq!(
        read(&cookies(&[b"a=1", b"b=2", b"\tsession=tok"])),
        Presented::One("tok")
    );
}

#[test]
fn a_quoted_value_loses_one_pair_of_quotes() {
    assert_eq!(read(&cookies(&[b"session=\"tok\""])), Presented::One("tok"));
    assert_eq!(read(&cookies(&[b"session=\"tok"])), Presented::One("\"tok"));
}

#[test]
fn an_empty_value_is_unreadable() {
    assert_eq!(read(&cookies(&[b"session="])), Presented::Unreadable);
    assert_eq!(read(&cookies(&[b"session=\"\""])), Presented::Unreadable);
}

#[test]
fn the_name_matches_exactly() {
    assert_eq!(read(&cookies(&[b"xsession=1"])), Presented::Absent);
    assert_eq!(read(&cookies(&[b"Session=1"])), Presented::Absent);
    assert_eq!(read(&cookies(&[b"session"])), Presented::Absent);
    assert_eq!(read(&cookies(&[b"session; xsession=1"])), Presented::Absent);
}

#[test]
fn a_repeated_name_is_repeated_within_or_across_fields() {
    assert_eq!(
        read(&cookies(&[b"session=a; session=b"])),
        Presented::Repeated
    );
    assert_eq!(
        read(&cookies(&[b"session=a", b"x=1; session=b"])),
        Presented::Repeated
    );
    assert_eq!(
        read(&cookies(&[b"session=a; session=a"])),
        Presented::Repeated
    );
}

#[test]
fn non_utf8_bytes_neither_hide_a_duplicate_nor_panic() {
    assert_eq!(
        read(&cookies(&[b"session=a; junk=\xff\xfe; session=b"])),
        Presented::Repeated
    );
    assert_eq!(
        read(&cookies(&[b"junk=\xff", b"session=b; session=c"])),
        Presented::Repeated
    );
    assert_eq!(
        read(&cookies(&[b"session=\xff\xfe"])),
        Presented::Unreadable
    );
    assert_eq!(
        read(&cookies(&[b"junk=\xff; session=ok"])),
        Presented::One("ok")
    );
}

#[test]
fn arbitrary_bytes_do_not_panic() {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..2000 {
        let field: Vec<u8> = (0..24)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                const ALPHABET: &[u8] = b"s=;\" \t\xff\x80ession";
                ALPHABET[(state % ALPHABET.len() as u64) as usize]
            })
            .collect();
        if let Ok(value) = HeaderValue::from_bytes(&field) {
            let mut headers = HeaderMap::new();
            headers.append(header::COOKIE, value);
            let _ = read(&headers);
        }
    }
}

fn with_origin(origin: &[&[u8]], host: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for value in origin {
        headers.append(header::ORIGIN, HeaderValue::from_bytes(value).unwrap());
    }
    if let Some(host) = host {
        headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
    }
    headers
}

fn with_proto(mut headers: HeaderMap, proto: &[u8]) -> HeaderMap {
    headers.insert("x-forwarded-proto", HeaderValue::from_bytes(proto).unwrap());
    headers
}

#[test]
fn the_cookie_slot_follows_the_origin_rule() {
    let allowed = |origin: &[&[u8]], host| cookie_slot_allowed(&with_origin(origin, host));
    let allowed_https = |origin: &[&[u8]], host| {
        cookie_slot_allowed(&with_proto(with_origin(origin, host), b"https"))
    };
    assert!(allowed(&[], None));
    assert!(allowed(&[], Some("gw.example")));
    assert!(allowed_https(&[b"https://gw.example"], Some("gw.example")));
    assert!(allowed_https(
        &[b"https://gw.example:443"],
        Some("gw.example")
    ));
    assert!(allowed(&[b"http://127.0.0.1:8080"], Some("127.0.0.1:8080")));
    assert!(!allowed_https(
        &[b"https://evil.example"],
        Some("gw.example")
    ));
    assert!(!allowed_https(
        &[b"https://gw.example:8443"],
        Some("gw.example")
    ));
    assert!(!allowed(&[b"null"], Some("gw.example")));
    assert!(!allowed(&[b"not a url"], Some("gw.example")));
    assert!(!allowed_https(
        &[b"https://\xff.example"],
        Some("gw.example")
    ));
    assert!(!allowed_https(
        &[b"https://gw.example", b"https://gw.example"],
        Some("gw.example")
    ));
    assert!(!allowed_https(&[b"https://gw.example"], None));
}

#[test]
fn the_cookie_slot_requires_the_origin_scheme_to_match_the_request() {
    let origin = |origin: &'static [u8], proto: Option<&[u8]>| {
        let headers = with_origin(&[origin], Some("gw.example"));
        cookie_slot_allowed(&match proto {
            Some(proto) => with_proto(headers, proto),
            None => headers,
        })
    };
    // The reported bypass: an http Origin against a request that arrived over https.
    assert!(!origin(b"http://gw.example", Some(b"https")));
    assert!(!origin(b"https://gw.example", None));
    assert!(origin(b"https://gw.example", Some(b"https")));
    assert!(origin(b"http://gw.example", None));
    assert!(origin(b"https://gw.example", Some(b" HTTPS ")));
    // Only the first forwarded value counts.
    assert!(origin(b"https://gw.example", Some(b"https, http")));
    assert!(!origin(b"http://gw.example", Some(b"https, http")));
    // Unrecognised or unreadable values fail closed.
    assert!(!origin(b"http://gw.example", Some(b"ftp")));
    assert!(!origin(b"http://gw.example", Some(b"\xff")));
}
