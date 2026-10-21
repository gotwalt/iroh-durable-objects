use ctutils::CtEq;
use noq_proto::crypto;
use rand::RngExt;
use rustls::crypto::cipher::{
    AeadKey, InboundOpaqueMessage, Iv, NONCE_LEN, OutboundPlainMessage, Tls13AeadAlgorithm,
};

/// Implements [`crypto::HandshakeTokenKey`] using a [`Tls13AeadAlgorithm`].
///
/// This can be obtained from looking through available ciphers from a
/// [`rustls::crypto::CryptoProvider`].
pub(crate) struct RustlsTokenKey {
    key: [u8; 32],
    aead: &'static dyn Tls13AeadAlgorithm,
}

impl RustlsTokenKey {
    /// Constructs [`crypto::HandshakeTokenKey`] from a [`rustls::crypto::CryptoProvider`].
    ///
    /// Tries to find a suitable TLS 1.3 cipher suite from the provided crypto provider,
    /// then uses it to extract an AEAD to use as the token key encryption method.
    ///
    /// Then generates a random master key to use.
    ///
    /// Returns `None` when this can't find a suitable TLS cipher suite in the given crypto
    /// provider.
    pub(crate) fn new(
        rng: &mut impl rand::CryptoRng,
        crypto_provider: &rustls::crypto::CryptoProvider,
    ) -> Option<Self> {
        let suite = crypto_provider
            .cipher_suites
            .iter()
            .filter_map(|suite| suite.tls13())
            .next()?;
        let aead = suite.aead_alg;
        Some(Self {
            key: rng.random(),
            aead,
        })
    }
}

impl crypto::HandshakeTokenKey for RustlsTokenKey {
    fn seal(&self, token_nonce: u128, data: &mut Vec<u8>) -> Result<(), crypto::CryptoError> {
        let key = AeadKey::from(self.key);
        let nonce: [u8; NONCE_LEN] = *token_nonce
            .to_le_bytes()
            .first_chunk()
            .expect("expected u128 > 96 bit");
        let iv = Iv::from(nonce);
        let msg = OutboundPlainMessage {
            typ: rustls::ContentType::ApplicationData,
            version: rustls::ProtocolVersion::TLSv1_3,
            payload: rustls::crypto::cipher::OutboundChunks::Single(&*data),
        };
        let out = self
            .aead
            .encrypter(key, iv)
            .encrypt(msg, 0)
            .map_err(|_| crypto::CryptoError)?;

        data.clear();
        data.extend(out.payload.as_ref());

        Ok(())
    }

    fn open<'a>(
        &self,
        token_nonce: u128,
        data: &'a mut [u8],
    ) -> Result<&'a [u8], crypto::CryptoError> {
        let key = AeadKey::from(self.key);
        let nonce: [u8; NONCE_LEN] = *token_nonce
            .to_le_bytes()
            .first_chunk()
            .expect("expected u128 > 96 bit");
        let iv = Iv::from(nonce);

        let msg = InboundOpaqueMessage::new(
            rustls::ContentType::ApplicationData,
            rustls::ProtocolVersion::TLSv1_3,
            data,
        );
        let plain = self
            .aead
            .decrypter(key, iv)
            .decrypt(msg, 0)
            .map_err(|_| crypto::CryptoError)?;

        Ok(plain.payload)
    }
}

pub(crate) struct Blake3HmacKey([u8; 32]);

impl Blake3HmacKey {
    pub(crate) fn new(rng: &mut impl rand::CryptoRng) -> Self {
        let mut key = [0u8; 32];
        rng.fill_bytes(&mut key);
        Self(key)
    }
}

impl crypto::HmacKey for Blake3HmacKey {
    fn sign(&self, data: &[u8], signature_out: &mut [u8]) {
        signature_out.copy_from_slice(blake3::keyed_hash(&self.0, data).as_slice());
    }

    fn signature_len(&self) -> usize {
        blake3::OUT_LEN // 32 bytes
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), crypto::CryptoError> {
        let reference = blake3::keyed_hash(&self.0, data);
        // to_bool is fine here, because it's the last thing we do to
        // distinguish success or failure (see to_bool documentation)
        if signature.ct_eq(reference.as_slice()).to_bool() {
            Ok(())
        } else {
            Err(crypto::CryptoError)
        }
    }
}

/// Secret from which an endpoint derives the keys that make stateless resets work across
/// restarts.
///
/// noq only sends a stateless reset for a short-header packet whose destination connection
/// ID its CID generator recognises, and peers only accept a reset whose token was computed
/// with the reset key of the endpoint that issued the CID. Both keys are therefore derived
/// from this one secret, so they cannot get out of sync.
#[derive(Clone)]
pub(crate) struct StatelessResetSecret([u8; 32]);

impl std::fmt::Debug for StatelessResetSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StatelessResetSecret(..)")
    }
}

impl StatelessResetSecret {
    pub(crate) fn new(secret: [u8; 32]) -> Self {
        Self(secret)
    }

    /// The key that signs stateless reset tokens.
    pub(crate) fn reset_key(&self) -> Blake3HmacKey {
        Blake3HmacKey(blake3::derive_key(
            "iroh 2026-10-21 stateless reset key",
            &self.0,
        ))
    }

    /// The key of the [`noq_proto::HashedConnectionIdGenerator`].
    pub(crate) fn connection_id_key(&self) -> u64 {
        let key = blake3::derive_key("iroh 2026-10-21 connection id key", &self.0);
        u64::from_le_bytes(*key.first_chunk().expect("32 > 8"))
    }
}

#[cfg(test)]
mod tests {
    use noq_proto::crypto::HmacKey;

    use super::*;

    #[test]
    fn stateless_reset_secret_is_deterministic_and_domain_separated() {
        let a = StatelessResetSecret::new([7; 32]);
        let b = StatelessResetSecret::new([7; 32]);
        let c = StatelessResetSecret::new([8; 32]);

        let sign = |s: &StatelessResetSecret| {
            let mut out = [0u8; 32];
            s.reset_key().sign(b"cid", &mut out);
            out
        };
        assert_eq!(sign(&a), sign(&b));
        assert_ne!(sign(&a), sign(&c));
        assert_eq!(a.connection_id_key(), b.connection_id_key());
        assert_ne!(a.connection_id_key(), c.connection_id_key());
    }
}
