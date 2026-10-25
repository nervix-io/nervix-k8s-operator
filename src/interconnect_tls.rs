//! The interconnect credentials of every NervixCluster.
//!
//! Nervix nodes authenticate all node-to-node traffic with mutual TLS. Each node presents a
//! certificate signed by its cluster's CA, naming the node in exactly one
//! `nervix://cluster/<cluster-id>/node/<node-id>` URI and naming the host it advertises. The
//! operator generates one CA per NervixCluster and keeps it in a Secret that only the operator
//! reads. It issues every node its certificate into a second Secret, which the pods mount, and
//! issues it again once it no longer names its node, was signed by another CA or nears its
//! expiry. A node reloads changed credentials without a restart.

use std::{collections::BTreeMap, string::FromUtf8Error};

use k8s_openapi::{ByteString, api::core::v1::Secret};
use kube::ResourceExt;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType,
};
use thiserror::Error;
use time::{Duration, OffsetDateTime};
use x509_parser::{
    certificate::X509Certificate, error::X509Error, extensions::GeneralName, nom,
    pem::parse_x509_pem, x509::SubjectPublicKeyInfo,
};

use crate::{api::NervixCluster, manifests};

/// Where the pods mount the credentials Secret.
pub const MOUNT_PATH: &str = "/etc/nervix/interconnect";
/// The credentials Secret's file holding the CA certificate every node trusts.
pub const CA_FILE: &str = "ca.pem";
/// The suffix of the file holding a node's certificate, after its node ID.
pub const CERTIFICATE_SUFFIX: &str = ".crt";
/// The suffix of the file holding a node's private key, after its node ID.
pub const PRIVATE_KEY_SUFFIX: &str = ".key";

const SECRET_COMPONENT: &str = "interconnect-tls";
/// The CA Secret holds the CA certificate and its key under the `kubernetes.io/tls` keys.
const AUTHORITY_SECRET_TYPE: &str = "kubernetes.io/tls";
const AUTHORITY_CERTIFICATE_KEY: &str = "tls.crt";
const AUTHORITY_PRIVATE_KEY_KEY: &str = "tls.key";
const CREDENTIALS_SECRET_TYPE: &str = "Opaque";

const AUTHORITY_VALIDITY: Duration = Duration::days(3650);
const NODE_VALIDITY: Duration = Duration::days(365);
/// A node certificate is issued again once less than this much of its validity remains.
const NODE_RENEWAL: Duration = Duration::days(30);
/// Certificates are valid from this long before they are issued, so that a node whose clock runs
/// behind the operator's accepts them as soon as they arrive.
const CLOCK_SKEW_ALLOWANCE: Duration = Duration::hours(1);

/// The bytes of a cluster ID that an identity URI keeps as they are: RFC 3986's unreserved
/// characters. Every other byte is percent-encoded, and the server decodes the segment back.
const IDENTITY_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

type X509ParseError = nom::Err<X509Error>;

#[derive(Debug, Error)]
pub enum CredentialsError {
    #[error("the interconnect CA Secret {secret} has no {key}")]
    MissingAuthorityData { secret: String, key: &'static str },
    #[error("the {key} of the interconnect CA Secret {secret} is not PEM text: {source}")]
    AuthorityNotText {
        secret: String,
        key: &'static str,
        #[source]
        source: FromUtf8Error,
    },
    #[error("the interconnect CA Secret {secret} does not hold a usable CA: {source}")]
    InvalidAuthority {
        secret: String,
        #[source]
        source: rcgen::Error,
    },
    #[error(
        "the private key in the interconnect CA Secret {secret} does not belong to its certificate"
    )]
    AuthorityKeyMismatch { secret: String },
    #[error("failed to generate interconnect credentials: {0}")]
    Generation(#[source] rcgen::Error),
}

pub fn authority_secret_name(cluster: &NervixCluster) -> String {
    format!("{}-interconnect-ca", cluster.name_any())
}

pub fn credentials_secret_name(cluster: &NervixCluster) -> String {
    format!("{}-interconnect-tls", cluster.name_any())
}

/// The CA that signs the interconnect certificates of one NervixCluster.
pub struct ClusterAuthority {
    certificate_pem: String,
    certificate_der: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
}

impl ClusterAuthority {
    /// Generates a new CA, valid from `now`.
    pub fn generate(now: OffsetDateTime) -> Result<Self, CredentialsError> {
        let key =
            KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(CredentialsError::Generation)?;
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, "Nervix interconnect CA");
        let mut params = CertificateParams::default();
        params.distinguished_name = distinguished_name;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = now - CLOCK_SKEW_ALLOWANCE;
        params.not_after = now + AUTHORITY_VALIDITY;

        let certificate = params
            .self_signed(&key)
            .map_err(CredentialsError::Generation)?;
        Ok(Self {
            certificate_pem: certificate.pem(),
            certificate_der: certificate.der().to_vec(),
            issuer: Issuer::new(params, key),
        })
    }

    /// Reads the CA that [`ClusterAuthority::secret`] stored.
    pub fn from_secret(secret: &Secret) -> Result<Self, CredentialsError> {
        let name = secret.name_any();
        let certificate_pem = secret_text(secret, AUTHORITY_CERTIFICATE_KEY)?;
        let private_key_pem = secret_text(secret, AUTHORITY_PRIVATE_KEY_KEY)?;
        let invalid = |source| CredentialsError::InvalidAuthority {
            secret: name.clone(),
            source,
        };

        let key = KeyPair::from_pem(&private_key_pem).map_err(invalid)?;
        let certificate_der = match parse_x509_pem(certificate_pem.as_bytes()) {
            Ok((_, pem)) => pem.contents,
            Err(_) => return Err(invalid(rcgen::Error::CouldNotParseCertificate)),
        };
        let Ok(certificate) = parse_certificate(&certificate_der) else {
            return Err(invalid(rcgen::Error::CouldNotParseCertificate));
        };
        if certificate.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
            return Err(CredentialsError::AuthorityKeyMismatch { secret: name });
        }
        let issuer =
            Issuer::from_ca_cert_der(&certificate_der.as_slice().into(), key).map_err(invalid)?;
        Ok(Self {
            certificate_pem,
            certificate_der,
            issuer,
        })
    }

    /// The Secret that keeps this CA for `cluster`. Only the operator reads it.
    pub fn secret(&self, cluster: &NervixCluster) -> Secret {
        let data = BTreeMap::from([
            (
                AUTHORITY_CERTIFICATE_KEY.to_string(),
                ByteString(self.certificate_pem.clone().into_bytes()),
            ),
            (
                AUTHORITY_PRIVATE_KEY_KEY.to_string(),
                ByteString(self.issuer.key().serialize_pem().into_bytes()),
            ),
        ]);
        Secret {
            metadata: manifests::child_metadata(
                cluster,
                &authority_secret_name(cluster),
                manifests::component_labels(cluster, SECRET_COMPONENT),
            ),
            type_: Some(AUTHORITY_SECRET_TYPE.to_string()),
            data: Some(data),
            ..Default::default()
        }
    }

    /// The Secret every node of `cluster` mounts: this CA's certificate and each node's
    /// certificate and private key. It keeps the credentials in `current` that still serve their
    /// node and issues the rest, valid from `now`.
    pub fn credentials_secret(
        &self,
        cluster: &NervixCluster,
        namespace: &str,
        current: Option<&Secret>,
        now: OffsetDateTime,
    ) -> Result<Secret, CredentialsError> {
        let authority_certificate = parse_certificate(&self.certificate_der)
            .map_err(|_| CredentialsError::Generation(rcgen::Error::CouldNotParseCertificate))?;
        let current_data = current.and_then(|secret| secret.data.as_ref());

        let mut data = BTreeMap::new();
        data.insert(
            CA_FILE.to_string(),
            ByteString(self.certificate_pem.clone().into_bytes()),
        );
        for ordinal in 0..cluster.spec.normalized_replicas() {
            let identity = NodeIdentity::new(cluster, namespace, ordinal);
            let kept = current_data
                .and_then(|data| NodeCredentials::from_data(data, &identity))
                .filter(|credentials| {
                    credentials.serves(&identity, authority_certificate.public_key(), now)
                });
            let credentials = match kept {
                Some(credentials) => credentials,
                None => self.issue(&identity, now)?,
            };
            data.insert(
                identity.certificate_file(),
                ByteString(credentials.certificate_pem.into_bytes()),
            );
            data.insert(
                identity.private_key_file(),
                ByteString(credentials.private_key_pem.into_bytes()),
            );
        }

        Ok(Secret {
            metadata: manifests::child_metadata(
                cluster,
                &credentials_secret_name(cluster),
                manifests::component_labels(cluster, SECRET_COMPONENT),
            ),
            type_: Some(CREDENTIALS_SECRET_TYPE.to_string()),
            data: Some(data),
            ..Default::default()
        })
    }

    fn issue(
        &self,
        identity: &NodeIdentity,
        now: OffsetDateTime,
    ) -> Result<NodeCredentials, CredentialsError> {
        let key =
            KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(CredentialsError::Generation)?;
        let dns_name = identity
            .dns_name
            .clone()
            .try_into()
            .map_err(CredentialsError::Generation)?;
        let uri = identity
            .uri
            .clone()
            .try_into()
            .map_err(CredentialsError::Generation)?;
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, identity.node_id.clone());
        let mut params = CertificateParams::default();
        params.distinguished_name = distinguished_name;
        params.subject_alt_names = vec![SanType::DnsName(dns_name), SanType::URI(uri)];
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.use_authority_key_identifier_extension = true;
        params.not_before = now - CLOCK_SKEW_ALLOWANCE;
        params.not_after = now + NODE_VALIDITY;

        let certificate = params
            .signed_by(&key, &self.issuer)
            .map_err(CredentialsError::Generation)?;
        Ok(NodeCredentials {
            certificate_pem: certificate.pem(),
            private_key_pem: key.serialize_pem(),
        })
    }
}

/// Who one node is to its peers: the node ID its pod starts with, the identity URI its
/// certificate must carry, and the host name it advertises its interconnect listener at.
struct NodeIdentity {
    node_id: String,
    uri: String,
    dns_name: String,
}

impl NodeIdentity {
    fn new(cluster: &NervixCluster, namespace: &str, ordinal: i32) -> Self {
        let node_id = manifests::node_id(ordinal);
        let cluster_id = cluster.spec.resolved_cluster_id(&cluster.name_any());
        let uri = format!(
            "nervix://cluster/{}/node/{node_id}",
            utf8_percent_encode(&cluster_id, IDENTITY_SEGMENT)
        );
        Self {
            dns_name: manifests::pod_fqdn(namespace, cluster, ordinal),
            uri,
            node_id,
        }
    }

    fn certificate_file(&self) -> String {
        format!("{}{CERTIFICATE_SUFFIX}", self.node_id)
    }

    fn private_key_file(&self) -> String {
        format!("{}{PRIVATE_KEY_SUFFIX}", self.node_id)
    }

    /// Whether `certificate` names exactly this node: its identity URI and its DNS name, and no
    /// other subject alternative name.
    fn is_named_by(&self, certificate: &X509Certificate<'_>) -> bool {
        let Ok(Some(names)) = certificate.subject_alternative_name() else {
            return false;
        };
        let mut uris = Vec::new();
        let mut dns_names = Vec::new();
        for name in &names.value.general_names {
            match name {
                GeneralName::URI(uri) => uris.push(*uri),
                GeneralName::DNSName(dns_name) => dns_names.push(*dns_name),
                _ => return false,
            }
        }
        uris == [self.uri.as_str()] && dns_names == [self.dns_name.as_str()]
    }
}

/// One node's certificate and private key, PEM-encoded.
struct NodeCredentials {
    certificate_pem: String,
    private_key_pem: String,
}

impl NodeCredentials {
    fn from_data(data: &BTreeMap<String, ByteString>, identity: &NodeIdentity) -> Option<Self> {
        let certificate = data.get(&identity.certificate_file())?;
        let private_key = data.get(&identity.private_key_file())?;
        Some(Self {
            certificate_pem: String::from_utf8(certificate.0.clone()).ok()?,
            private_key_pem: String::from_utf8(private_key.0.clone()).ok()?,
        })
    }

    /// Whether these credentials can stay in service for `identity`: the key belongs to the
    /// certificate, the certificate was signed by the CA whose key is `authority_key`, names
    /// exactly this node and stays valid for longer than the renewal period after `now`.
    fn serves(
        &self,
        identity: &NodeIdentity,
        authority_key: &SubjectPublicKeyInfo<'_>,
        now: OffsetDateTime,
    ) -> bool {
        let Ok((_, pem)) = parse_x509_pem(self.certificate_pem.as_bytes()) else {
            return false;
        };
        let Ok(certificate) = parse_certificate(&pem.contents) else {
            return false;
        };
        let Ok(key) = KeyPair::from_pem(&self.private_key_pem) else {
            return false;
        };
        if certificate.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
            return false;
        }
        if certificate.verify_signature(Some(authority_key)).is_err() {
            return false;
        }
        if !identity.is_named_by(&certificate) {
            return false;
        }
        let renewal_starts = (now + NODE_RENEWAL).unix_timestamp();
        certificate.validity().not_after.timestamp() > renewal_starts
    }
}

fn parse_certificate(der: &[u8]) -> Result<X509Certificate<'_>, X509ParseError> {
    let (_, certificate) = x509_parser::parse_x509_certificate(der)?;
    Ok(certificate)
}

fn secret_text(secret: &Secret, key: &'static str) -> Result<String, CredentialsError> {
    let Some(value) = secret.data.as_ref().and_then(|data| data.get(key)) else {
        return Err(CredentialsError::MissingAuthorityData {
            secret: secret.name_any(),
            key,
        });
    };
    String::from_utf8(value.0.clone()).map_err(|source| CredentialsError::AuthorityNotText {
        secret: secret.name_any(),
        key,
        source,
    })
}

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use x509_parser::extensions::ParsedExtension;

    use crate::api::NervixClusterSpec;

    use super::*;

    const NAMESPACE: &str = "nervix";

    #[test]
    fn node_certificates_name_their_node_and_chain_to_the_cluster_authority() {
        let now = OffsetDateTime::now_utc();
        let cluster = cluster(Some("nervix-kube"), 3);
        let authority = ClusterAuthority::generate(now).expect("a CA is generated");

        let secret = authority
            .credentials_secret(&cluster, NAMESPACE, None, now)
            .expect("credentials are issued");

        let data = secret.data.as_ref().expect("the Secret has data");
        assert_eq!(
            data.keys().collect::<Vec<_>>(),
            vec![
                "ca.pem",
                "node-1.crt",
                "node-1.key",
                "node-2.crt",
                "node-2.key",
                "node-3.crt",
                "node-3.key"
            ]
        );
        assert_eq!(data["ca.pem"].0, authority.certificate_pem.as_bytes());

        let authority_der = pem_contents(&data["ca.pem"]);
        let authority_certificate = parse_certificate(&authority_der).expect("the CA parses");
        assert!(authority_certificate.is_ca());

        for (ordinal, node_id) in ["node-1", "node-2", "node-3"].into_iter().enumerate() {
            let der = pem_contents(&data[&format!("{node_id}.crt")]);
            let certificate = parse_certificate(&der).expect("the node certificate parses");
            certificate
                .verify_signature(Some(authority_certificate.public_key()))
                .expect("the cluster CA signed the node certificate");
            assert!(!certificate.is_ca());
            assert_eq!(
                subject_alternative_names(&certificate),
                vec![
                    format!("DNS:nervix-{ordinal}.nervix-headless.nervix.svc.cluster.local"),
                    format!("URI:nervix://cluster/nervix-kube/node/{node_id}"),
                ]
            );
            let usage = certificate
                .extended_key_usage()
                .expect("the extended key usage parses")
                .expect("the certificate has an extended key usage")
                .value;
            assert!(usage.server_auth && usage.client_auth);
            assert!(certificate.validity().is_valid());

            let key = KeyPair::from_pem(
                &String::from_utf8(data[&format!("{node_id}.key")].0.clone())
                    .expect("the key is PEM text"),
            )
            .expect("the private key parses");
            assert_eq!(
                certificate.public_key().subject_public_key.data.as_ref(),
                key.public_key_raw()
            );
        }
    }

    #[test]
    fn the_identity_uri_percent_encodes_a_cluster_id_outside_the_unreserved_characters() {
        let now = OffsetDateTime::now_utc();
        let cluster = cluster(Some("prod cluster/eu"), 1);
        let authority = ClusterAuthority::generate(now).expect("a CA is generated");

        let secret = authority
            .credentials_secret(&cluster, NAMESPACE, None, now)
            .expect("credentials are issued");

        let der = pem_contents(&secret.data.as_ref().expect("the Secret has data")["node-1.crt"]);
        let certificate = parse_certificate(&der).expect("the node certificate parses");
        assert!(
            subject_alternative_names(&certificate)
                .contains(&"URI:nervix://cluster/prod%20cluster%2Feu/node/node-1".to_string())
        );
    }

    #[test]
    fn the_authority_secret_keeps_the_ca_for_the_operator() {
        let now = OffsetDateTime::now_utc();
        let cluster = cluster(None, 3);
        let authority = ClusterAuthority::generate(now).expect("a CA is generated");
        let issued = authority
            .credentials_secret(&cluster, NAMESPACE, None, now)
            .expect("credentials are issued");

        let secret = authority.secret(&cluster);
        assert_eq!(secret.name_any(), "nervix-interconnect-ca");
        assert_eq!(secret.type_.as_deref(), Some("kubernetes.io/tls"));
        assert_owned_by_cluster(&secret);

        let restored = ClusterAuthority::from_secret(&secret).expect("the CA reads back");
        assert_eq!(restored.certificate_pem, authority.certificate_pem);
        let reissued = restored
            .credentials_secret(&cluster, NAMESPACE, None, now)
            .expect("the restored CA issues credentials");
        let authority_der = pem_contents(&issued.data.as_ref().expect("data")["ca.pem"]);
        let authority_certificate = parse_certificate(&authority_der).expect("the CA parses");
        let der = pem_contents(&reissued.data.as_ref().expect("data")["node-1.crt"]);
        parse_certificate(&der)
            .expect("the node certificate parses")
            .verify_signature(Some(authority_certificate.public_key()))
            .expect("the restored CA signs as the stored one");
    }

    #[test]
    fn an_authority_secret_whose_key_belongs_to_another_ca_is_refused() {
        let now = OffsetDateTime::now_utc();
        let cluster = cluster(None, 3);
        let authority = ClusterAuthority::generate(now).expect("a CA is generated");
        let other = ClusterAuthority::generate(now).expect("another CA is generated");
        let mut secret = authority.secret(&cluster);
        let other_key = other.secret(&cluster).data.expect("data")["tls.key"].clone();
        secret
            .data
            .as_mut()
            .expect("data")
            .insert("tls.key".to_string(), other_key);

        let refused = ClusterAuthority::from_secret(&secret)
            .err()
            .expect("a CA whose key does not match is refused");
        assert!(matches!(
            refused,
            CredentialsError::AuthorityKeyMismatch { .. }
        ));
    }

    #[test]
    fn credentials_that_serve_their_node_are_kept() {
        let now = OffsetDateTime::now_utc();
        let cluster = cluster(None, 3);
        let authority = ClusterAuthority::generate(now).expect("a CA is generated");
        let issued = authority
            .credentials_secret(&cluster, NAMESPACE, None, now)
            .expect("credentials are issued");

        let reconciled = authority
            .credentials_secret(&cluster, NAMESPACE, Some(&issued), now)
            .expect("credentials are reconciled");

        assert_eq!(reconciled.data, issued.data);
        assert_eq!(reconciled.name_any(), "nervix-interconnect-tls");
        assert_eq!(reconciled.type_.as_deref(), Some("Opaque"));
        assert_owned_by_cluster(&reconciled);
    }

    #[test]
    fn credentials_signed_by_another_ca_are_issued_again() {
        let now = OffsetDateTime::now_utc();
        let cluster = cluster(None, 3);
        let previous = ClusterAuthority::generate(now).expect("a CA is generated");
        let issued = previous
            .credentials_secret(&cluster, NAMESPACE, None, now)
            .expect("credentials are issued");
        let replacement = ClusterAuthority::generate(now).expect("another CA is generated");

        let reconciled = replacement
            .credentials_secret(&cluster, NAMESPACE, Some(&issued), now)
            .expect("credentials are reconciled");

        let data = reconciled.data.as_ref().expect("data");
        assert_eq!(data["ca.pem"].0, replacement.certificate_pem.as_bytes());
        let authority_der = pem_contents(&data["ca.pem"]);
        let authority_certificate = parse_certificate(&authority_der).expect("the CA parses");
        for node_id in ["node-1", "node-2", "node-3"] {
            let file = format!("{node_id}.crt");
            assert_ne!(data[&file], issued.data.as_ref().expect("data")[&file]);
            let der = pem_contents(&data[&file]);
            parse_certificate(&der)
                .expect("the node certificate parses")
                .verify_signature(Some(authority_certificate.public_key()))
                .expect("the replacement CA signed the node certificate");
        }
    }

    #[test]
    fn credentials_for_another_cluster_identity_are_issued_again() {
        let now = OffsetDateTime::now_utc();
        let authority = ClusterAuthority::generate(now).expect("a CA is generated");
        let issued = authority
            .credentials_secret(&cluster(Some("staging"), 3), NAMESPACE, None, now)
            .expect("credentials are issued");

        let reconciled = authority
            .credentials_secret(
                &cluster(Some("production"), 3),
                NAMESPACE,
                Some(&issued),
                now,
            )
            .expect("credentials are reconciled");

        let der = pem_contents(&reconciled.data.as_ref().expect("data")["node-2.crt"]);
        let certificate = parse_certificate(&der).expect("the node certificate parses");
        assert!(
            subject_alternative_names(&certificate)
                .contains(&"URI:nervix://cluster/production/node/node-2".to_string())
        );
    }

    #[test]
    fn credentials_are_issued_again_once_they_enter_the_renewal_period() {
        let now = OffsetDateTime::now_utc();
        let cluster = cluster(None, 1);
        let renewal_boundary = now + NODE_RENEWAL - NODE_VALIDITY;
        let authority =
            ClusterAuthority::generate(renewal_boundary - Duration::days(1)).expect("a CA");
        let outside = authority
            .credentials_secret(
                &cluster,
                NAMESPACE,
                None,
                renewal_boundary + Duration::days(1),
            )
            .expect("credentials are issued");
        let inside = authority
            .credentials_secret(
                &cluster,
                NAMESPACE,
                None,
                renewal_boundary - Duration::days(1),
            )
            .expect("credentials are issued");

        let kept = authority
            .credentials_secret(&cluster, NAMESPACE, Some(&outside), now)
            .expect("credentials are reconciled");
        let renewed = authority
            .credentials_secret(&cluster, NAMESPACE, Some(&inside), now)
            .expect("credentials are reconciled");

        assert_eq!(kept.data, outside.data);
        let renewed_data = renewed.data.as_ref().expect("data");
        assert_ne!(
            renewed_data["node-1.crt"],
            inside.data.as_ref().expect("data")["node-1.crt"]
        );
        let der = pem_contents(&renewed_data["node-1.crt"]);
        let not_after = parse_certificate(&der)
            .expect("the renewed certificate parses")
            .validity()
            .not_after
            .timestamp();
        assert!(not_after > (now + NODE_VALIDITY - Duration::days(1)).unix_timestamp());
    }

    #[test]
    fn scaling_issues_credentials_for_new_nodes_and_drops_removed_ones() {
        let now = OffsetDateTime::now_utc();
        let authority = ClusterAuthority::generate(now).expect("a CA is generated");
        let three = authority
            .credentials_secret(&cluster(None, 3), NAMESPACE, None, now)
            .expect("credentials are issued");

        let five = authority
            .credentials_secret(&cluster(None, 5), NAMESPACE, Some(&three), now)
            .expect("credentials are reconciled");
        let one = authority
            .credentials_secret(&cluster(None, 1), NAMESPACE, Some(&five), now)
            .expect("credentials are reconciled");

        let three_data = three.data.as_ref().expect("data");
        let five_data = five.data.as_ref().expect("data");
        assert_eq!(five_data.len(), 11);
        for file in ["node-1.crt", "node-2.key", "node-3.crt"] {
            assert_eq!(five_data[file], three_data[file]);
        }
        assert!(five_data.contains_key("node-5.key"));
        assert_eq!(
            one.data.as_ref().expect("data").keys().collect::<Vec<_>>(),
            vec!["ca.pem", "node-1.crt", "node-1.key"]
        );
        assert_eq!(
            one.data.as_ref().expect("data")["node-1.crt"],
            three_data["node-1.crt"]
        );
    }

    fn cluster(cluster_id: Option<&str>, replicas: i32) -> NervixCluster {
        let mut cluster = NervixCluster::new(
            "nervix",
            NervixClusterSpec {
                image: "ghcr.io/nervix-io/nervix:test".to_string(),
                replicas,
                cluster_id: cluster_id.map(str::to_string),
                initial_default_user_password_secret_ref: None,
                storage: "20Gi".to_string(),
                log_filter: None,
                local_access: None,
                resources: Default::default(),
            },
        );
        cluster.metadata = ObjectMeta {
            name: Some("nervix".to_string()),
            namespace: Some(NAMESPACE.to_string()),
            uid: Some("4a0b3c4d-5e6f-4a1b-8c2d-3e4f5a6b7c8d".to_string()),
            ..Default::default()
        };
        cluster
    }

    fn assert_owned_by_cluster(secret: &Secret) {
        let owners = secret
            .metadata
            .owner_references
            .as_ref()
            .expect("the Secret has an owner");
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].kind, "NervixCluster");
        assert_eq!(owners[0].name, "nervix");
        assert_eq!(owners[0].controller, Some(true));
        assert_eq!(secret.metadata.namespace.as_deref(), Some(NAMESPACE));
        assert_eq!(
            secret
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("app.kubernetes.io/component"))
                .map(String::as_str),
            Some("interconnect-tls")
        );
    }

    fn pem_contents(value: &ByteString) -> Vec<u8> {
        let (_, pem) = parse_x509_pem(&value.0).expect("the value is a PEM certificate");
        pem.contents
    }

    fn subject_alternative_names(certificate: &X509Certificate<'_>) -> Vec<String> {
        let mut names = Vec::new();
        for extension in certificate.extensions() {
            if let ParsedExtension::SubjectAlternativeName(alternative_names) =
                extension.parsed_extension()
            {
                for name in &alternative_names.general_names {
                    match name {
                        GeneralName::DNSName(dns_name) => names.push(format!("DNS:{dns_name}")),
                        GeneralName::URI(uri) => names.push(format!("URI:{uri}")),
                        other => names.push(format!("{other:?}")),
                    }
                }
            }
        }
        names
    }
}
