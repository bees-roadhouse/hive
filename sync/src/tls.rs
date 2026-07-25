// The mTLS carrier (PLAN-v2.1 PR 4.5, D29/D30): how two devices prove which
// device they are before a session frame is written.
//
// The decision this module records (the 4.5 spike's job, open decision 10 in
// docs/DIRECTION.md): **self-signed certificates carrying the device's
// ed25519 key, pinned by SPKI** — not rustls' raw-public-key mode, and not a
// CA of our own. Reasoning, in the order it mattered:
//
//   * Both ends must already know each other. Enrollment (PR 4.7) hands the
//     node the device's public keys and the device the node's; from then on
//     the trust decision is byte equality against a pinned key. A PKI would
//     add an issuer to protect, a chain to validate, and an expiry to renew —
//     three failure modes for a question we can answer with `==`.
//   * Raw public keys (RFC 7250) express that directly, but need BOTH peers
//     to negotiate the extension, have no representation any ordinary tool
//     can dump, and would leave a field diagnosis of "handshake failed" with
//     nothing to look at. A self-signed cert is inspectable with openssl and
//     legible in a packet capture, at the cost of ~40 lines of rcgen.
//   * So the certificate is a CARRIER, never a trust root. Nothing here
//     validates a chain, checks a name, or looks at an expiry — the pin is
//     the whole verdict, in both directions (the listener pins its enrolled
//     devices; the dialer pins the node it enrolled with, which is what makes
//     a spoofed SRV answer harmless at PR 4.8).
//
// Stack: ed25519 + x25519 from the dalek suite (`hive_core::identity` — one
// asymmetric introduction for identity, control signing, and sharing), rcgen
// for minting, rustls with the ring provider for the handshake. The device
// key never leaves `DeviceIdentity`; rcgen sees it as PKCS#8 for exactly as
// long as it takes to sign the certificate.
//
// Everything here is generic over `AsyncRead + AsyncWrite` like the rest of
// the crate, so a session runs over TLS, over a bare socket, or over an
// in-memory duplex without noticing — which is what lets `tests/loopback.rs`
// and `tests/tls_loopback.rs` share a protocol.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use hive_core::identity::DeviceIdentity;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig,
    SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// The SNI name every hive dialer sends. It is deliberately a reserved
/// `.invalid` name (RFC 2606) that resolves nowhere and that no verifier in
/// this crate ever reads: the peer's identity is its key, and a name-based
/// check would be a second, weaker answer to a question already settled.
pub const PINNED_SERVER_NAME: &str = "device.hive.invalid";

/// ASN.1 prefix of an RFC 8410 Ed25519 SubjectPublicKeyInfo:
/// `SEQUENCE { SEQUENCE { OID 1.3.101.112 }, BIT STRING (0 unused bits) }`.
/// Frozen by the standard, which is why a pin can be computed from a bare
/// 32-byte public key without minting a certificate first.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// A device's transport identity as a peer sees it: blake3 of the DER
/// SubjectPublicKeyInfo the certificate carries.
///
/// Hashed rather than stored raw so one comparison covers the key *and* its
/// algorithm identifier — a pin can never be satisfied by the same 32 bytes
/// presented as some other key type. The node stores these in node-meta
/// (PR 4.7) and the desktop stores the node's beside its address (PR 4.8).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpkiPin([u8; 32]);

impl SpkiPin {
    /// Pin a DER SubjectPublicKeyInfo directly.
    pub fn from_spki_der(spki: &[u8]) -> Self {
        Self(*blake3::hash(spki).as_bytes())
    }

    /// Pin an enrolled device from its bare ed25519 public key — the form
    /// enrollment carries (PR 4.7's `{device_id, ed25519_pk, x25519_pk}`),
    /// with no certificate in hand.
    pub fn from_ed25519_public(public: &[u8; 32]) -> Self {
        let mut spki = Vec::with_capacity(ED25519_SPKI_PREFIX.len() + 32);
        spki.extend_from_slice(&ED25519_SPKI_PREFIX);
        spki.extend_from_slice(public);
        Self::from_spki_der(&spki)
    }

    /// Pin the certificate a peer actually presented. This is the ONE X.509
    /// read the transport performs, and it is a read of the key only —
    /// no chain, no name, no validity window.
    pub fn from_cert_der(cert: &CertificateDer<'_>) -> Result<Self> {
        let parsed = webpki::EndEntityCert::try_from(cert)
            .map_err(|e| anyhow!("peer certificate does not parse: {e}"))?;
        Ok(Self::from_spki_der(
            parsed.subject_public_key_info().as_ref(),
        ))
    }

    /// The device's own pin, from its identity.
    pub fn of(identity: &DeviceIdentity) -> Self {
        Self::from_ed25519_public(&identity.ed25519_public())
    }

    /// Round-trip through storage (node-meta, config files).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Lowercase hex, so a pin in a log line greps against the one in node-meta.
impl std::fmt::Display for SpkiPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", data_encoding::HEXLOWER.encode(&self.0))
    }
}

impl std::fmt::Debug for SpkiPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SpkiPin({self})")
    }
}

/// Which devices a config will talk to. A trait and not a set because the
/// node's answer changes while it runs: PR 4.7 backs this with node-meta so a
/// `device revoke` takes effect on the next handshake without a restart.
pub trait PinnedKeys: std::fmt::Debug + Send + Sync {
    fn accepts(&self, pin: &SpkiPin) -> bool;
}

/// The fixed answer: a set decided when the config was built. What a dialer
/// uses (one node, one pin) and what tests use.
#[derive(Debug, Clone, Default)]
pub struct PinSet(BTreeSet<SpkiPin>);

impl PinSet {
    pub fn new(pins: impl IntoIterator<Item = SpkiPin>) -> Self {
        Self(pins.into_iter().collect())
    }

    /// One peer — the dialer's case.
    pub fn one(pin: SpkiPin) -> Self {
        Self::new([pin])
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PinnedKeys for PinSet {
    fn accepts(&self, pin: &SpkiPin) -> bool {
        self.0.contains(pin)
    }
}

/// A device's transport certificate: its ed25519 identity wrapped in the
/// X.509 the handshake needs, plus the pin a peer will compute for it.
///
/// Minting is a pure function of the identity — rcgen derives the serial and
/// the key id from the public key, and the validity window is fixed — so a
/// device produces byte-identical certificates on every boot and nothing has
/// to be persisted.
pub struct DeviceCert {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pin: SpkiPin,
}

impl DeviceCert {
    /// Mint the self-signed certificate for this device.
    pub fn self_signed(identity: &DeviceIdentity) -> Result<Self> {
        let pkcs8 = identity
            .ed25519_pkcs8_der()
            .context("exporting the device signing key for the transport certificate")?;
        let key = PrivatePkcs8KeyDer::from(pkcs8);
        let signing = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&key, &rcgen::PKCS_ED25519)
            .map_err(|e| anyhow!("rcgen rejected the device signing key: {e}"))?;

        let mut params = rcgen::CertificateParams::new(vec![PINNED_SERVER_NAME.to_string()])
            .map_err(|e| anyhow!("building certificate parameters: {e}"))?;
        // The subject is documentation for whoever dumps the cert, never an
        // input to a decision: the pin is the identity.
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            format!("hive device {}", SpkiPin::of(identity)),
        );
        // A device both dials and listens, so it needs both usages. Nothing
        // in this tree checks them; a foreign TLS stack looking at the cert
        // should still see something coherent.
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        // rcgen's default window (1975..4096) stands deliberately: expiry is
        // a PKI answer to key rotation, and ours is revocation — an unpinned
        // key is refused the moment node-meta says so (PR 4.7), which no
        // notAfter date could do better.
        let cert = params
            .self_signed(&signing)
            .map_err(|e| anyhow!("self-signing the device certificate: {e}"))?;

        Ok(Self {
            cert: cert.der().clone(),
            key,
            pin: SpkiPin::of(identity),
        })
    }

    /// What a peer will pin.
    pub fn pin(&self) -> SpkiPin {
        self.pin
    }

    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert
    }

    fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

/// Public halves only — the private key never reaches a log line.
impl std::fmt::Debug for DeviceCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCert")
            .field("pin", &self.pin)
            .finish()
    }
}

/// The one TLS backend in this tree (the workspace's jmap/reqwest line picks
/// ring too). Selected explicitly rather than through rustls' process-global
/// default, so a library elsewhere installing another provider cannot change
/// what a hive handshake does.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Client and server verifier in one type: both sides ask the same question
/// ("is the key this peer presented one I pinned?") and both get it wrong in
/// the same ways if it is written twice.
#[derive(Debug)]
struct PinnedPeerVerifier {
    pins: Arc<dyn PinnedKeys>,
    provider: Arc<CryptoProvider>,
}

impl PinnedPeerVerifier {
    /// The verdict, shared by both directions. A device presents exactly one
    /// self-signed certificate: intermediates would mean a chain, and a chain
    /// is a trust model we deliberately do not have.
    fn check(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<SpkiPin, rustls::Error> {
        if !intermediates.is_empty() {
            return Err(rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ));
        }
        let pin = SpkiPin::from_cert_der(end_entity)
            .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::BadEncoding))?;
        if !self.pins.accepts(&pin) {
            tracing::debug!(%pin, "refusing an unpinned device key");
            return Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(pin)
    }
}

impl ServerCertVerifier for PinnedPeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        // The name is not the identity (see PINNED_SERVER_NAME): a hive node
        // is reached at whatever address won the PR 4.8 race, under whatever
        // name that candidate had, and it is the key that says who answered.
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        // No validity window to be inside — see DeviceCert::self_signed.
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ClientCertVerifier for PinnedPeerVerifier {
    /// No CA subjects to hint at — a client picks its one device cert.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Dialer config: presents `cert`, accepts only listeners whose key is in
/// `expect`. TLS 1.3 only — both ends of this protocol ship together, so
/// there is no legacy peer to accommodate and no downgrade to reason about.
pub fn client_config(cert: &DeviceCert, expect: Arc<dyn PinnedKeys>) -> Result<ClientConfig> {
    let provider = provider();
    let verifier = Arc::new(PinnedPeerVerifier {
        pins: expect,
        provider: provider.clone(),
    });
    ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3 for the dialer")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert.cert_der().clone()], cert.private_key())
        .context("installing the device certificate on the dialer")
}

/// Listener config: presents `cert`, requires client auth, accepts only
/// devices whose key is in `allow`.
pub fn server_config(cert: &DeviceCert, allow: Arc<dyn PinnedKeys>) -> Result<ServerConfig> {
    let provider = provider();
    let verifier = Arc::new(PinnedPeerVerifier {
        pins: allow,
        provider: provider.clone(),
    });
    ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3 for the listener")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert.cert_der().clone()], cert.private_key())
        .context("installing the device certificate on the listener")
}

/// A reusable dialer. Build once and race many candidates with it (PR 4.8) —
/// the config holds no per-connection state.
pub fn connector(cert: &DeviceCert, expect: Arc<dyn PinnedKeys>) -> Result<TlsConnector> {
    Ok(TlsConnector::from(Arc::new(client_config(cert, expect)?)))
}

/// A reusable acceptor — one per listener (PR 4.7).
pub fn acceptor(cert: &DeviceCert, allow: Arc<dyn PinnedKeys>) -> Result<TlsAcceptor> {
    Ok(TlsAcceptor::from(Arc::new(server_config(cert, allow)?)))
}

/// Complete the client half of a handshake, returning the stream and the pin
/// of the peer that answered — the caller needs that to know which node it
/// reached, and the session that follows is scoped to it.
pub async fn connect_pinned<S>(
    connector: &TlsConnector,
    stream: S,
) -> Result<(tokio_rustls::client::TlsStream<S>, SpkiPin)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let name = ServerName::try_from(PINNED_SERVER_NAME).expect("the constant is a valid DNS name");
    let stream = connector
        .connect(name, stream)
        .await
        .context("TLS handshake with a pinned peer failed (dialer side)")?;
    let pin = peer_pin(stream.get_ref().1)?;
    Ok((stream, pin))
}

/// Complete the server half, returning the stream and the pin of the device
/// that connected — PR 4.7's listener authorizes the session with it.
pub async fn accept_pinned<S>(
    acceptor: &TlsAcceptor,
    stream: S,
) -> Result<(tokio_rustls::server::TlsStream<S>, SpkiPin)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let stream = acceptor
        .accept(stream)
        .await
        .context("TLS handshake with a pinned peer failed (listener side)")?;
    let pin = peer_pin(stream.get_ref().1)?;
    Ok((stream, pin))
}

/// The authenticated peer's pin, after a handshake. Absent certificates are
/// impossible on both sides of this config (client auth is mandatory), so
/// their absence is a bug, not a policy question.
fn peer_pin(conn: &rustls::CommonState) -> Result<SpkiPin> {
    let Some([end_entity, ..]) = conn.peer_certificates() else {
        bail!("peer completed the handshake without presenting a certificate");
    };
    SpkiPin::from_cert_der(end_entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_core::identity::DeviceIdentity;

    /// The two ways to compute a pin must agree: from the bare public key
    /// (what enrollment stores) and from the certificate (what the handshake
    /// sees). If these ever diverged, every enrolled device would be refused.
    #[test]
    fn a_pin_from_the_key_equals_the_pin_from_the_certificate() {
        let identity = DeviceIdentity::from_seed(&[0x21; 32]);
        let cert = DeviceCert::self_signed(&identity).unwrap();
        assert_eq!(
            SpkiPin::from_cert_der(cert.cert_der()).unwrap(),
            SpkiPin::from_ed25519_public(&identity.ed25519_public())
        );
        assert_eq!(cert.pin(), SpkiPin::of(&identity));
        // And the pin survives storage: node-meta keeps the 32 bytes (PR 4.7).
        assert_eq!(SpkiPin::from_bytes(*cert.pin().as_bytes()), cert.pin());
    }

    /// Client auth is not optional. A listener that OFFERED it without
    /// REQUIRING it would hand an unauthenticated peer a session, and both
    /// answers are rustls trait defaults — so the defaults we lean on are
    /// pinned here rather than assumed.
    #[test]
    fn the_listener_requires_client_authentication() {
        let verifier = PinnedPeerVerifier {
            pins: Arc::new(PinSet::default()),
            provider: provider(),
        };
        assert!(ClientCertVerifier::offer_client_auth(&verifier));
        assert!(ClientCertVerifier::client_auth_mandatory(&verifier));
        assert!(
            ClientCertVerifier::root_hint_subjects(&verifier).is_empty(),
            "no CA subjects to hint at — a device picks its one certificate"
        );
    }

    /// Minting is deterministic, so nothing about the certificate needs to be
    /// persisted or re-pinned across a restart.
    #[test]
    fn minting_twice_yields_the_same_certificate() {
        let identity = DeviceIdentity::from_seed(&[0x22; 32]);
        let first = DeviceCert::self_signed(&identity).unwrap();
        let second = DeviceCert::self_signed(&identity).unwrap();
        assert_eq!(first.cert_der(), second.cert_der());
    }

    #[test]
    fn different_devices_pin_differently() {
        let alpha = DeviceCert::self_signed(&DeviceIdentity::from_seed(&[1; 32])).unwrap();
        let beta = DeviceCert::self_signed(&DeviceIdentity::from_seed(&[2; 32])).unwrap();
        assert_ne!(alpha.pin(), beta.pin());
        assert_ne!(alpha.cert_der(), beta.cert_der());
    }

    #[test]
    fn a_pin_set_answers_only_for_what_it_holds() {
        let alpha = SpkiPin::of(&DeviceIdentity::from_seed(&[1; 32]));
        let beta = SpkiPin::of(&DeviceIdentity::from_seed(&[2; 32]));
        let pins = PinSet::one(alpha);
        assert!(pins.accepts(&alpha));
        assert!(!pins.accepts(&beta));
        assert!(PinSet::default().is_empty());
    }

    /// Garbage is an Err, never a panic: the pin is computed from bytes a
    /// peer chose (HOSTILE-DECODE, docs/TESTING-STRATEGY.md §2).
    #[test]
    fn a_malformed_certificate_is_an_error() {
        for bytes in [
            vec![],
            vec![0x30],
            vec![0x30, 0x82, 0xff, 0xff],
            b"-----BEGIN CERTIFICATE-----".to_vec(),
            vec![0xff; 512],
        ] {
            let der = CertificateDer::from(bytes);
            assert!(SpkiPin::from_cert_der(&der).is_err());
        }
    }

    /// Secret material must not reach a log line through a stray `{:?}`.
    #[test]
    fn the_debug_impl_prints_the_pin_only() {
        let identity = DeviceIdentity::from_seed(&[0x23; 32]);
        let cert = DeviceCert::self_signed(&identity).unwrap();
        let rendered = format!("{cert:?}");
        assert!(rendered.contains(&cert.pin().to_string()));
        assert!(!rendered.contains("2323"), "{rendered}");
    }
}
