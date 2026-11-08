use super::*;
use rustls::{ClientConnection, ServerConnection, pki_types::ServerName};
use std::io::Cursor;
use x509_parser::{extensions::GeneralName, parse_x509_certificate};

fn peer_certificate(authority: &Authority, name: &str) -> CertificateDer<'static> {
    let mut roots = RootCertStore::empty();
    roots.add(authority.certificate.clone()).unwrap();
    let client_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut client = ClientConnection::new(
        Arc::new(client_config),
        ServerName::try_from(name.to_owned()).unwrap(),
    )
    .unwrap();
    let mut server = ServerConnection::new(authority.server(name).unwrap()).unwrap();
    for _ in 0..8 {
        let mut bytes = Vec::new();
        client.write_tls(&mut bytes).unwrap();
        server.read_tls(&mut Cursor::new(bytes)).unwrap();
        server.process_new_packets().unwrap();
        let mut bytes = Vec::new();
        server.write_tls(&mut bytes).unwrap();
        client.read_tls(&mut Cursor::new(bytes)).unwrap();
        client.process_new_packets().unwrap();
        if !client.is_handshaking() && !server.is_handshaking() {
            return client.peer_certificates().unwrap()[0].clone();
        }
    }
    panic!("TLS handshake did not finish");
}

#[test]
fn root_has_product_name_ten_year_validity_and_fresh_keys() {
    let first = Authority::new().unwrap();
    let second = Authority::new().unwrap();
    assert_ne!(first.certificate, second.certificate);
    let (_, certificate) = parse_x509_certificate(&first.certificate).unwrap();
    let name = certificate
        .subject()
        .iter_common_name()
        .next()
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(name, "AgentTomo Root CA");
    assert_eq!(
        certificate.validity().not_after.to_datetime().year(),
        time::OffsetDateTime::now_utc().year() + 10
    );
    assert!(certificate.is_ca());
}

#[test]
fn wildcard_certificate_has_matching_cn_sans_and_one_day_validity() {
    let authority = Authority::new().unwrap();
    let der = peer_certificate(&authority, "www.baidu.com");
    let (_, certificate) = parse_x509_certificate(&der).unwrap();
    let name = certificate
        .subject()
        .iter_common_name()
        .next()
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(name, "*.baidu.com");
    let san = certificate.subject_alternative_name().unwrap().unwrap();
    assert_eq!(
        san.value.general_names,
        vec![
            GeneralName::DNSName("*.baidu.com"),
            GeneralName::DNSName("baidu.com")
        ]
    );
    let remaining = certificate.validity().not_after.timestamp()
        - time::OffsetDateTime::now_utc().unix_timestamp();
    assert!((86390..=86400).contains(&remaining));
    assert!(certificate.extensions().iter().any(|extension| matches!(
        extension.parsed_extension(),
        x509_parser::extensions::ParsedExtension::AuthorityKeyIdentifier(_)
    )));
    assert_eq!(peer_certificate(&authority, "a.baidu.com"), der);
    assert_eq!(peer_certificate(&authority, "baidu.com"), der);
}

#[test]
fn cache_groups_dns_names_at_public_and_private_suffix_boundaries() {
    let authority = Authority::new().unwrap();
    for group in [
        vec!["baidu.com", "www.baidu.com", "A.BAIDU.COM."],
        vec!["x.a.baidu.com", "y.a.baidu.com"],
        vec!["example.co.uk", "www.example.co.uk"],
        vec!["alice.github.io", "www.alice.github.io"],
        vec!["city.kawasaki.jp", "www.city.kawasaki.jp"],
        vec!["xn--bcher-kva.de", "WWW.XN--BCHER-KVA.DE"],
        vec!["localhost", "LOCALHOST."],
        vec!["2001:db8::1", "2001:0db8:0:0:0:0:0:1"],
    ] {
        let config = authority.server(group[0]).unwrap();
        for name in &group[1..] {
            assert!(
                Arc::ptr_eq(&config, &authority.server(name).unwrap()),
                "{group:?}"
            );
        }
    }
    for (first, second) in [
        ("www.baidu.com", "x.a.baidu.com"),
        ("alice.github.io", "bob.github.io"),
        ("example.co.uk", "other.co.uk"),
        ("a.internal", "b.internal"),
        ("127.0.0.1", "127.0.0.2"),
    ] {
        assert!(!Arc::ptr_eq(
            &authority.server(first).unwrap(),
            &authority.server(second).unwrap()
        ));
    }
    peer_certificate(&authority, "localhost");
    peer_certificate(&authority, "127.0.0.1");
    peer_certificate(&authority, "2001:db8::1");
    for invalid in [
        "",
        "*.baidu.com",
        "https://baidu.com",
        "a..baidu.com",
        "a.baidu.com..",
    ] {
        assert!(authority.server(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn concurrent_requests_share_one_certificate_within_one_run() {
    let authority = Arc::new(Authority::new().unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|index| {
            let authority = authority.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                authority
                    .server(&format!("client{index}.baidu.com"))
                    .unwrap()
            })
        })
        .collect();
    let configs: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(
        configs
            .iter()
            .all(|config| Arc::ptr_eq(config, &configs[0]))
    );
    let other_run = Authority::new().unwrap();
    assert!(!Arc::ptr_eq(
        &configs[0],
        &other_run.server("www.baidu.com").unwrap()
    ));
}

#[test]
fn cache_expires_from_insertion_and_enforces_its_capacity() {
    let mut authority = Authority::new().unwrap();
    assert_eq!(
        authority.servers.policy().time_to_live(),
        Some(Duration::from_secs(3600))
    );
    assert_eq!(authority.servers.policy().time_to_idle(), None);
    assert_eq!(authority.servers.policy().max_capacity(), Some(1024));
    authority.servers = Cache::builder()
        .max_capacity(2)
        .time_to_live(Duration::from_millis(20))
        .build();
    let first = authority.server("www.baidu.com").unwrap();
    std::thread::sleep(Duration::from_millis(40));
    let renewed = authority.server("www.baidu.com").unwrap();
    assert!(!Arc::ptr_eq(&first, &renewed));

    authority.servers = Cache::builder().max_capacity(2).build();
    for name in [
        "a.example.com",
        "a.example.net",
        "a.example.org",
        "a.baidu.com",
    ] {
        authority.server(name).unwrap();
    }
    authority.servers.run_pending_tasks();
    assert!(authority.servers.entry_count() <= 2);
    // A client already holding a TLS config can still use it after eviction.
    ServerConnection::new(first).unwrap();
}

#[test]
fn root_validity_uses_calendar_years_including_leap_days() {
    let date = |year, month, day| {
        time::Date::from_calendar_date(year, month, day)
            .unwrap()
            .with_hms(12, 34, 56)
            .unwrap()
            .assume_utc()
    };
    assert_eq!(
        root_expiry(date(2024, time::Month::February, 29)).unwrap(),
        date(2034, time::Month::February, 28)
    );
    assert_eq!(
        root_expiry(date(2026, time::Month::November, 6)).unwrap(),
        date(2036, time::Month::November, 6)
    );
}
