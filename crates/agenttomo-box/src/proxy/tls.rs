//! Task-local signing material. Private keys never leave the collector's memory.
use moka::sync::Cache;
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, Issuer, KeyPair};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, DnsName, PrivatePkcs8KeyDer},
};
use std::{io, net::IpAddr, sync::Arc, time::Duration};
use time::OffsetDateTime;

#[cfg(test)]
mod tests;

pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    certificate: CertificateDer<'static>,
    pub pem: String,
    pub client: Arc<ClientConfig>,
    servers: Cache<String, Arc<ServerConfig>>,
}

fn validity(params: &mut CertificateParams, now: OffsetDateTime, expires: OffsetDateTime) {
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = expires;
}

fn root_expiry(now: OffsetDateTime) -> Result<OffsetDateTime, time::error::ComponentRange> {
    let year = now.year() + 10;
    // A February 29 anniversary falls on February 28 in a non-leap year.
    now.replace_year(year)
        .or_else(|_| now.replace_day(28)?.replace_year(year))
}

fn certificate_name(hostname: &str) -> io::Result<String> {
    let name = hostname
        .strip_suffix('.')
        .unwrap_or(hostname)
        .to_ascii_lowercase();
    if let Ok(address) = name.parse::<IpAddr>() {
        return Ok(address.to_string());
    }
    DnsName::try_from(hostname).map_err(io::Error::other)?;
    let Some(domain) = psl::domain(name.as_bytes()).filter(|domain| domain.suffix().is_known())
    else {
        return Ok(name);
    };
    let base = if domain.as_bytes() == name.as_bytes() {
        name.as_str()
    } else {
        name.split_once('.').expect("subdomain has a parent").1
    };
    Ok(format!("*.{base}"))
}

impl Authority {
    pub fn new() -> io::Result<Self> {
        let mut roots = RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        roots.add_parsable_certificates(native.certs);
        if roots.is_empty() {
            return Err(io::Error::other(format!(
                "no trusted upstream CA certificates: {:?}",
                native.errors
            )));
        }
        let mut client = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut params = CertificateParams::default();
        let now = OffsetDateTime::now_utc();
        validity(
            &mut params,
            now,
            root_expiry(now).map_err(io::Error::other)?,
        );
        params.is_ca = rcgen::IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "AgentTomo Root CA");
        let key = KeyPair::generate().map_err(io::Error::other)?;
        let certificate = params.self_signed(&key).map_err(io::Error::other)?;
        Ok(Self {
            pem: certificate.pem(),
            certificate: certificate.der().clone(),
            issuer: Issuer::new(params, key),
            client: Arc::new(client),
            servers: Cache::builder()
                .max_capacity(1024)
                .time_to_live(Duration::from_secs(3600))
                .build(),
        })
    }

    pub fn server(&self, name: &str) -> io::Result<Arc<ServerConfig>> {
        let name = certificate_name(name)?;
        self.servers
            .try_get_with(name.clone(), || self.sign(&name))
            .map_err(|error| io::Error::other(error.to_string()))
    }

    fn sign(&self, name: &str) -> io::Result<Arc<ServerConfig>> {
        let mut names = vec![name.to_owned()];
        if let Some(base) = name.strip_prefix("*.") {
            names.push(base.to_owned());
        }
        // rcgen writes IP addresses as IP SANs and other names as DNS SANs.
        let mut params = CertificateParams::new(names).map_err(io::Error::other)?;
        let now = OffsetDateTime::now_utc();
        validity(&mut params, now, now + time::Duration::days(1));
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        // Required by strict X.509 clients (including Python 3.13 defaults).
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        let key = KeyPair::generate().map_err(io::Error::other)?;
        let certificate = params
            .signed_by(&key, &self.issuer)
            .map_err(io::Error::other)?;
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone(), self.certificate.clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .map_err(io::Error::other)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}
