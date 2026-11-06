//! Task-local signing material. Private keys never leave the collector's memory.
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, Issuer, KeyPair};
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer},
};
use std::{
    io,
    sync::Arc,
    time::{Duration, SystemTime},
};

pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    certificate: CertificateDer<'static>,
    pub pem: String,
    pub client: Arc<ClientConfig>,
}

fn validity(params: &mut CertificateParams) {
    let now = SystemTime::now();
    params.not_before = (now - Duration::from_secs(300)).into();
    params.not_after = (now + Duration::from_secs(24 * 3600)).into();
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
        validity(&mut params);
        params.is_ca = rcgen::IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(
            DnType::CommonName,
            format!("AgentTomo task {}", uuid::Uuid::new_v4()),
        );
        let key = KeyPair::generate().map_err(io::Error::other)?;
        let certificate = params.self_signed(&key).map_err(io::Error::other)?;
        Ok(Self {
            pem: certificate.pem(),
            certificate: certificate.der().clone(),
            issuer: Issuer::new(params, key),
            client: Arc::new(client),
        })
    }

    pub fn server(&self, name: &str) -> io::Result<Arc<ServerConfig>> {
        // rcgen recognizes IP SANs as well as DNS names. Never fall back to a
        // wildcard or a certificate for an unrelated host when SNI is absent.
        let mut params = CertificateParams::new(vec![name.to_owned()]).map_err(io::Error::other)?;
        validity(&mut params);
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
