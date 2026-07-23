//! A small helper on top of [`rusty_tls::TrustPolicy`] (re-exported at this
//! crate's root as [`crate::TrustPolicy`]) so pinning a private CA doesn't
//! require a caller to add their own dependency on `rustls`/
//! `rustls-pki-types` just to build a `CertificateDer`.

use rustls_pki_types::CertificateDer;
use rusty_tls::TrustPolicy;

/// Builds a [`TrustPolicy::PinnedAnchors`] from one or more raw,
/// DER-encoded root certificates -- verify `https://` connections against
/// exactly these roots, ignoring the system trust store entirely. For a
/// private CA or hermetic tests.
///
/// ```no_run
/// # fn read_ca_der() -> Vec<u8> { vec![] }
/// let ca_der = read_ca_der();
/// let policy = rusty_request::pinned_anchors([ca_der]);
/// let client = rusty_request::Client::builder().trust_policy(policy).build();
/// ```
pub fn pinned_anchors(der_certs: impl IntoIterator<Item = Vec<u8>>) -> TrustPolicy {
    TrustPolicy::PinnedAnchors(der_certs.into_iter().map(CertificateDer::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_anchors_wraps_each_der_cert() {
        let policy = pinned_anchors([vec![1, 2, 3], vec![4, 5, 6]]);
        let TrustPolicy::PinnedAnchors(certs) = policy else {
            panic!("expected TrustPolicy::PinnedAnchors");
        };
        assert_eq!(
            certs,
            vec![
                CertificateDer::from(vec![1, 2, 3]),
                CertificateDer::from(vec![4, 5, 6])
            ]
        );
    }

    #[test]
    fn pinned_anchors_of_empty_input_is_empty() {
        let policy = pinned_anchors(Vec::<Vec<u8>>::new());
        let TrustPolicy::PinnedAnchors(certs) = policy else {
            panic!("expected TrustPolicy::PinnedAnchors");
        };
        assert!(certs.is_empty());
    }
}
