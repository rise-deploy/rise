//! `oauthrelay_core::Sealer` that seals relay envelopes with a provider-wrapped data key.
//!
//! Each replica seals locally with AES-256-GCM under a random 32-byte data key. The data key is
//! encrypted ("wrapped") once through Rise's [`EncryptionProvider`] (AWS KMS or local AES-GCM)
//! under [`EncryptionContext::internal`]`("oauth-relay/data-key/v1")`, and the wrapped key
//! travels inside every envelope, so any replica can open any envelope by unwrapping it through
//! the same provider. A replica generates a fresh data key every [`ROTATION_PERIOD`]; envelopes
//! sealed under an earlier key keep opening for as long as the provider can unwrap it.
//!
//! # Envelope format
//!
//! ```text
//! rro1.{wrapped}.{sealed}
//!
//! wrapped = base64url(provider ciphertext of base64url(data key))
//! sealed  = base64url(nonce[12] ‖ AES-256-GCM(data key, nonce, plaintext, aad) ‖ tag[16])
//! aad     = "rise oauth-relay seal v1\0" ‖ SHA-256(wrapped bytes) ‖ associated_data
//! ```
//!
//! `base64url` is unpadded, so an envelope only contains `[A-Za-z0-9._-]` and is safe in URLs,
//! form bodies and refresh handles. The wrapped bytes are the provider ciphertext without its
//! `rise:enc:v2:` text prefix ([`encode_envelope`] restores it to unwrap). The inner AEAD binds
//! oauthrelay's associated data and the exact wrapped-key bytes, so neither a wrapped key from
//! another envelope nor another wrapping of the same data key can be spliced in; the provider's
//! context binds the data key to this purpose, so no other Rise ciphertext unwraps as one.
//!
//! The fixed overhead is the wrapped key plus 28 bytes of nonce and tag, base64url-encoded:
//! about 140 characters with the local provider and about 300 with AWS KMS. The plaintext grows
//! by 4/3 and has no size limit (the provider only ever sees the 43-character data key).
//!
//! # Failures
//!
//! A malformed envelope, a wrapped key the provider rejects as a ciphertext (tagged
//! [`CiphertextRejected`](rise_backend_core::encryption::CiphertextRejected)) and an inner
//! authentication failure are [`SealError::InvalidEnvelope`]. Any other provider failure, and a
//! provider call that does not finish within [`PROVIDER_TIMEOUT`], is
//! [`SealError::Unavailable`]: an envelope that is not known to be bad is never treated as bad,
//! so a key-service outage fails requests instead of signing users out.
//!
//! Unwrapped keys are cached by `SHA-256(wrapped)` for [`UNWRAPPED_KEY_TTL`], and concurrent
//! unseals of one wrapped key share a single provider call.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes_gcm::{
    aead::{Aead, Generate, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use oauthrelay_core::{SealError, Sealer};
use rand::Rng;
use rise_backend_core::encryption::{decode_envelope, encode_envelope, is_ciphertext_rejected};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::server::encryption::{EncryptionContext, EncryptionProvider};

/// Versioned prefix of every envelope this sealer produces.
const PREFIX: &str = "rro1.";
/// Domain tag at the start of the inner AEAD's associated data.
const AAD_DOMAIN: &[u8] = b"rise oauth-relay seal v1\0";
/// `EncryptionContext::internal` slot the data keys are wrapped under.
const DATA_KEY_SLOT: &str = "oauth-relay/data-key/v1";

/// How long a replica seals under one data key.
pub const ROTATION_PERIOD: Duration = Duration::from_secs(60 * 60);
/// Upper bound on every wrap and unwrap call to the encryption provider. It stays below
/// oauthrelay's own sealer timeout (5 s), so the relay sees `Unavailable` rather than a cut-off.
pub const PROVIDER_TIMEOUT: Duration = Duration::from_secs(3);
/// How long an unwrapped data key stays cached.
pub const UNWRAPPED_KEY_TTL: Duration = Duration::from_secs(60 * 60);
/// Most unwrapped data keys held at once; one is minted per replica and rotation period.
const UNWRAPPED_KEY_CAPACITY: u64 = 1024;
/// After a failed rotation, keep sealing under the outgoing key for this long before retrying.
const ROTATION_RETRY: Duration = Duration::from_secs(30);

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Larger wrapped keys are rejected before any provider call. A KMS ciphertext of a data key is
/// about 200 bytes; a local AES-GCM one is 71.
const MAX_WRAPPED_LEN: usize = 2048;

/// The data key this replica currently seals under.
struct DataKey {
    cipher: Arc<Aes256Gcm>,
    wrapped_b64: String,
    wrapped_digest: [u8; 32],
    created: Instant,
}

#[derive(Default)]
struct KeyState {
    current: Option<Arc<DataKey>>,
    /// Set after a failed rotation: until then, keep sealing under the outgoing key.
    retry_rotation_at: Option<Instant>,
}

/// Why an unwrap failed. Failures are not cached; concurrent waiters share one.
#[derive(Debug)]
enum UnwrapError {
    Rejected,
    Unavailable,
}

/// Seals and opens relay envelopes using Rise's configured encryption provider.
pub struct RiseSealer {
    provider: Arc<dyn EncryptionProvider>,
    /// Never held across an `.await`.
    state: Mutex<KeyState>,
    /// Serializes wraps so that one replica mints one data key per rotation. Only a seal that
    /// has no usable key waits on it; others keep sealing under the outgoing key.
    rotation: tokio::sync::Mutex<()>,
    unwrapped: moka::future::Cache<[u8; 32], Arc<Aes256Gcm>>,
}

impl RiseSealer {
    pub fn new(provider: Arc<dyn EncryptionProvider>) -> Self {
        Self {
            provider,
            state: Mutex::default(),
            rotation: tokio::sync::Mutex::new(()),
            unwrapped: moka::future::Cache::builder()
                .max_capacity(UNWRAPPED_KEY_CAPACITY)
                .time_to_live(UNWRAPPED_KEY_TTL)
                .build(),
        }
    }

    fn context() -> EncryptionContext {
        EncryptionContext::internal(DATA_KEY_SLOT)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, KeyState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_fresh(&self, key: &DataKey, now: Instant) -> bool {
        now.duration_since(key.created) < ROTATION_PERIOD
    }

    /// The key to seal under, rotating it when it has expired.
    async fn data_key(&self) -> Result<Arc<DataKey>, SealError> {
        let now = Instant::now();
        let outgoing = {
            let state = self.state();
            match &state.current {
                Some(key) if self.is_fresh(key, now) => return Ok(key.clone()),
                Some(key) if state.retry_rotation_at.is_some_and(|at| now < at) => {
                    return Ok(key.clone())
                }
                current => current.clone(),
            }
        };

        let Some(outgoing) = outgoing else {
            // Nothing to seal under yet: wait for the single in-flight wrap, or do it.
            return tokio::time::timeout(PROVIDER_TIMEOUT, async {
                let _rotation = self.rotation.lock().await;
                let current = self.state().current.clone();
                if let Some(key) = current {
                    return Ok(key);
                }
                let key = self.wrap_new_key().await?;
                self.install(key.clone(), None).await;
                Ok(key)
            })
            .await
            .unwrap_or_else(|_| {
                tracing::warn!("Timed out waiting for the OAuth relay data key");
                Err(SealError::Unavailable)
            });
        };

        // Expired: one seal rotates while the others keep using the outgoing key.
        let Ok(_rotation) = self.rotation.try_lock() else {
            return Ok(outgoing);
        };
        let current = self.state().current.clone();
        if let Some(key) = current.filter(|key| !Arc::ptr_eq(key, &outgoing)) {
            return Ok(key);
        }
        match self.wrap_new_key().await {
            Ok(key) => {
                self.install(key.clone(), Some(outgoing)).await;
                Ok(key)
            }
            Err(_) => {
                self.state().retry_rotation_at = Some(Instant::now() + ROTATION_RETRY);
                Ok(outgoing)
            }
        }
    }

    async fn install(&self, key: Arc<DataKey>, outgoing: Option<Arc<DataKey>>) {
        {
            let mut state = self.state();
            state.current = Some(key.clone());
            state.retry_rotation_at = None;
        }
        // This replica opens its own envelopes without a provider call; refreshing the outgoing
        // key keeps the envelopes it sealed recently cheap to open, too.
        for key in std::iter::once(key).chain(outgoing) {
            self.unwrapped
                .insert(key.wrapped_digest, key.cipher.clone())
                .await;
        }
    }

    /// Mints a data key and wraps it through the provider.
    async fn wrap_new_key(&self) -> Result<Arc<DataKey>, SealError> {
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        let cipher = Aes256Gcm::new_from_slice(&key).expect("AES-256 key is 32 bytes");
        let encoded = URL_SAFE_NO_PAD.encode(key);
        let wrapped = match tokio::time::timeout(
            PROVIDER_TIMEOUT,
            self.provider.encrypt(&encoded, &Self::context()),
        )
        .await
        {
            Ok(Ok(wrapped)) => wrapped,
            Ok(Err(error)) => {
                tracing::warn!("Failed to wrap an OAuth relay data key: {:?}", error);
                return Err(SealError::Unavailable);
            }
            Err(_) => {
                tracing::warn!("Timed out wrapping an OAuth relay data key");
                return Err(SealError::Unavailable);
            }
        };
        let wrapped = decode_envelope(&wrapped).map_err(|error| {
            tracing::error!(
                "Encryption provider returned an unexpected envelope for the OAuth relay data key: {:?}",
                error
            );
            SealError::Unavailable
        })?;
        Ok(Arc::new(DataKey {
            cipher: Arc::new(cipher),
            wrapped_b64: URL_SAFE_NO_PAD.encode(&wrapped),
            wrapped_digest: Sha256::digest(&wrapped).into(),
            created: Instant::now(),
        }))
    }

    /// The data key `wrapped` holds, from the cache or through the provider.
    async fn unwrap_key(
        &self,
        wrapped: &[u8],
        digest: [u8; 32],
    ) -> Result<Arc<Aes256Gcm>, SealError> {
        let unwrap = async {
            let plaintext = match self
                .provider
                .decrypt(&encode_envelope(wrapped), &Self::context())
                .await
            {
                Ok(plaintext) => plaintext,
                Err(error) if is_ciphertext_rejected(&error) => {
                    tracing::debug!("OAuth relay data key rejected: {:?}", error);
                    return Err(UnwrapError::Rejected);
                }
                Err(error) => {
                    tracing::warn!("Failed to unwrap an OAuth relay data key: {:?}", error);
                    return Err(UnwrapError::Unavailable);
                }
            };
            let key = URL_SAFE_NO_PAD
                .decode(plaintext)
                .ok()
                .and_then(|key| Aes256Gcm::new_from_slice(&key).ok())
                .ok_or_else(|| {
                    tracing::error!("An OAuth relay data key unwrapped to an invalid key");
                    UnwrapError::Rejected
                })?;
            Ok(Arc::new(key))
        };
        match tokio::time::timeout(
            PROVIDER_TIMEOUT,
            self.unwrapped.try_get_with(digest, unwrap),
        )
        .await
        {
            Ok(Ok(cipher)) => Ok(cipher),
            Ok(Err(error)) => Err(match *error {
                UnwrapError::Rejected => SealError::InvalidEnvelope,
                UnwrapError::Unavailable => SealError::Unavailable,
            }),
            Err(_) => {
                tracing::warn!("Timed out unwrapping an OAuth relay data key");
                Err(SealError::Unavailable)
            }
        }
    }
}

fn aad(wrapped_digest: &[u8; 32], associated_data: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_DOMAIN.len() + 32 + associated_data.len());
    aad.extend_from_slice(AAD_DOMAIN);
    aad.extend_from_slice(wrapped_digest);
    aad.extend_from_slice(associated_data);
    aad
}

/// Splits an envelope into its wrapped key and its `nonce ‖ ciphertext ‖ tag`.
fn parse(envelope: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let (wrapped, sealed) = envelope.strip_prefix(PREFIX)?.split_once('.')?;
    let wrapped = URL_SAFE_NO_PAD.decode(wrapped).ok()?;
    let sealed = URL_SAFE_NO_PAD.decode(sealed).ok()?;
    let well_formed =
        (1..=MAX_WRAPPED_LEN).contains(&wrapped.len()) && sealed.len() >= NONCE_LEN + TAG_LEN;
    well_formed.then_some((wrapped, sealed))
}

#[async_trait::async_trait]
impl Sealer for RiseSealer {
    async fn seal(&self, plaintext: &[u8], associated_data: &[u8]) -> Result<String, SealError> {
        let key = self.data_key().await?;
        let nonce = Nonce::generate();
        let ciphertext = key
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad(&key.wrapped_digest, associated_data),
                },
            )
            .map_err(|error| {
                tracing::error!("Failed to seal an OAuth relay envelope: {:?}", error);
                SealError::Unavailable
            })?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(format!(
            "{PREFIX}{}.{}",
            key.wrapped_b64,
            URL_SAFE_NO_PAD.encode(sealed)
        ))
    }

    async fn unseal(&self, envelope: &str, associated_data: &[u8]) -> Result<Vec<u8>, SealError> {
        let (wrapped, sealed) = parse(envelope).ok_or(SealError::InvalidEnvelope)?;
        let digest: [u8; 32] = Sha256::digest(&wrapped).into();
        let cipher = self.unwrap_key(&wrapped, digest).await?;
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_from(nonce).map_err(|_| SealError::InvalidEnvelope)?;
        cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: &aad(&digest, associated_data),
                },
            )
            .map_err(|_| SealError::InvalidEnvelope)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    use base64::engine::general_purpose::STANDARD;
    use rise_backend_core::encryption::CiphertextRejected;

    use super::*;
    use crate::server::encryption::providers::local::LocalEncryptionProvider;

    const WORKS: u8 = 0;
    const FAILS: u8 = 1;
    const HANGS: u8 = 2;
    const REJECTS: u8 = 3;

    /// A local provider that counts calls and can be made to fail, hang or reject ciphertexts.
    struct MockProvider {
        inner: LocalEncryptionProvider,
        mode: AtomicU8,
        encrypts: AtomicUsize,
        decrypts: AtomicUsize,
    }

    impl MockProvider {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: LocalEncryptionProvider::new(&STANDARD.encode([7u8; 32])).unwrap(),
                mode: AtomicU8::new(WORKS),
                encrypts: AtomicUsize::new(0),
                decrypts: AtomicUsize::new(0),
            })
        }

        fn set(&self, mode: u8) {
            self.mode.store(mode, Ordering::SeqCst);
        }

        fn encrypts(&self) -> usize {
            self.encrypts.load(Ordering::SeqCst)
        }

        fn decrypts(&self) -> usize {
            self.decrypts.load(Ordering::SeqCst)
        }

        async fn behave(&self) -> anyhow::Result<()> {
            // Yield so that concurrent callers interleave around the provider call.
            tokio::time::sleep(Duration::from_millis(10)).await;
            match self.mode.load(Ordering::SeqCst) {
                FAILS => anyhow::bail!("key service unreachable"),
                HANGS => std::future::pending().await,
                REJECTS => Err(CiphertextRejected::tag(anyhow::anyhow!("bad ciphertext"))),
                _ => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl EncryptionProvider for MockProvider {
        async fn encrypt(
            &self,
            plaintext: &str,
            context: &EncryptionContext,
        ) -> anyhow::Result<String> {
            self.encrypts.fetch_add(1, Ordering::SeqCst);
            if self.mode.load(Ordering::SeqCst) != REJECTS {
                self.behave().await?;
            }
            self.inner.encrypt(plaintext, context).await
        }

        async fn decrypt(
            &self,
            envelope: &str,
            context: &EncryptionContext,
        ) -> anyhow::Result<String> {
            self.decrypts.fetch_add(1, Ordering::SeqCst);
            self.behave().await?;
            self.inner.decrypt(envelope, context).await
        }
    }

    fn sealer(provider: &Arc<MockProvider>) -> RiseSealer {
        RiseSealer::new(provider.clone())
    }

    fn split(envelope: &str) -> (Vec<u8>, Vec<u8>) {
        parse(envelope).unwrap()
    }

    fn join(wrapped: &[u8], sealed: &[u8]) -> String {
        format!(
            "{PREFIX}{}.{}",
            URL_SAFE_NO_PAD.encode(wrapped),
            URL_SAFE_NO_PAD.encode(sealed)
        )
    }

    fn invalid(result: Result<Vec<u8>, SealError>) -> bool {
        matches!(result, Err(SealError::InvalidEnvelope))
    }

    fn unavailable<T>(result: Result<T, SealError>) -> bool {
        matches!(result, Err(SealError::Unavailable))
    }

    #[tokio::test]
    async fn round_trips_with_url_safe_envelopes_and_wrong_associated_data_fails() {
        let provider = MockProvider::new();
        let sealer = sealer(&provider);
        let envelope = sealer.seal(b"token response", b"relay/key").await.unwrap();
        assert!(envelope.starts_with("rro1."));
        assert!(envelope
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)));
        assert_eq!(
            sealer.unseal(&envelope, b"relay/key").await.unwrap(),
            b"token response"
        );
        assert!(invalid(sealer.unseal(&envelope, b"relay/other").await));
        assert!(invalid(sealer.unseal(&envelope, b"").await));
        // Another replica sharing the provider opens it, too.
        assert_eq!(
            RiseSealer::new(provider.clone())
                .unseal(&envelope, b"relay/key")
                .await
                .unwrap(),
            b"token response"
        );
        assert_eq!(
            sealer.seal(b"", b"").await.unwrap().len(),
            139,
            "fixed overhead with the local provider"
        );
    }

    #[tokio::test]
    async fn tampering_with_any_part_is_an_invalid_envelope() {
        let provider = MockProvider::new();
        let sealer = sealer(&provider);
        let envelope = sealer.seal(b"secret", b"ad").await.unwrap();
        let (wrapped, sealed) = split(&envelope);

        let mut nonce = sealed.clone();
        nonce[0] ^= 1;
        let mut ciphertext = sealed.clone();
        ciphertext[NONCE_LEN] ^= 1;
        let mut tag = sealed.clone();
        *tag.last_mut().unwrap() ^= 1;
        let mut bad_wrapped = wrapped.clone();
        bad_wrapped[20] ^= 1;
        for tampered in [
            join(&wrapped, &nonce),
            join(&wrapped, &ciphertext),
            join(&wrapped, &tag),
            join(&bad_wrapped, &sealed),
        ] {
            assert!(invalid(sealer.unseal(&tampered, b"ad").await), "{tampered}");
        }

        // A valid wrapped key from another envelope (another data key) does not fit.
        let other = RiseSealer::new(provider.clone())
            .seal(b"secret", b"ad")
            .await
            .unwrap();
        let (other_wrapped, _) = split(&other);
        assert_ne!(other_wrapped, wrapped);
        assert!(invalid(
            sealer.unseal(&join(&other_wrapped, &sealed), b"ad").await
        ));

        // Nor does another wrapping of the same data key: the AAD binds the wrapped bytes.
        let context = RiseSealer::context();
        let key = provider
            .inner
            .decrypt(&encode_envelope(&wrapped), &context)
            .await
            .unwrap();
        let rewrapped =
            decode_envelope(&provider.inner.encrypt(&key, &context).await.unwrap()).unwrap();
        assert_ne!(rewrapped, wrapped);
        assert!(invalid(
            sealer.unseal(&join(&rewrapped, &sealed), b"ad").await
        ));
        assert_eq!(sealer.unseal(&envelope, b"ad").await.unwrap(), b"secret");
    }

    #[tokio::test]
    async fn malformed_envelopes_are_invalid_without_a_provider_call() {
        let provider = MockProvider::new();
        let sealer = sealer(&provider);
        let envelope = sealer.seal(b"secret", b"ad").await.unwrap();
        let (wrapped, sealed) = split(&envelope);
        let body = &envelope[PREFIX.len()..];
        for malformed in [
            String::new(),
            "rro1.".into(),
            format!("rro2.{body}"),
            format!("v1.{body}"),
            body.to_string(),
            format!("{envelope}.extra"),
            format!("{envelope}="),
            envelope.replace('.', "!"),
            join(&[], &sealed),
            join(&wrapped, &sealed[..NONCE_LEN + TAG_LEN - 1]),
            join(&vec![0; MAX_WRAPPED_LEN + 1], &sealed),
            format!(
                "rro1.{}.{}",
                STANDARD.encode(&wrapped),
                STANDARD.encode(&sealed)
            ),
        ] {
            assert!(
                invalid(sealer.unseal(&malformed, b"ad").await),
                "{malformed}"
            );
        }
        assert_eq!(provider.decrypts(), 0);
    }

    #[tokio::test]
    async fn other_rise_ciphertexts_are_not_data_keys() {
        let provider = MockProvider::new();
        let sealer = sealer(&provider);
        let envelope = sealer.seal(b"secret", b"ad").await.unwrap();
        let (_, sealed) = split(&envelope);
        let key = URL_SAFE_NO_PAD.encode([1u8; 32]);
        for context in [
            EncryptionContext::internal("startup-test"),
            EncryptionContext::extension(uuid::Uuid::new_v4(), "oauth", "login", DATA_KEY_SLOT),
        ] {
            let foreign =
                decode_envelope(&provider.inner.encrypt(&key, &context).await.unwrap()).unwrap();
            assert!(invalid(
                sealer.unseal(&join(&foreign, &sealed), b"ad").await
            ));
        }
    }

    #[tokio::test]
    async fn a_failing_provider_is_unavailable_until_it_recovers() {
        let provider = MockProvider::new();
        let envelope = sealer(&provider).seal(b"secret", b"ad").await.unwrap();

        provider.set(FAILS);
        let replica = sealer(&provider);
        assert!(unavailable(replica.seal(b"secret", b"ad").await));
        assert!(unavailable(replica.unseal(&envelope, b"ad").await));

        provider.set(WORKS);
        assert_eq!(replica.unseal(&envelope, b"ad").await.unwrap(), b"secret");
        let sealed = replica.seal(b"again", b"ad").await.unwrap();
        assert_eq!(replica.unseal(&sealed, b"ad").await.unwrap(), b"again");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_provider_is_unavailable_within_the_timeout() {
        let provider = MockProvider::new();
        let envelope = sealer(&provider).seal(b"secret", b"ad").await.unwrap();
        provider.set(HANGS);
        let replica = sealer(&provider);

        let within_timeout = |started: Instant| {
            let elapsed = started.elapsed();
            elapsed >= PROVIDER_TIMEOUT && elapsed <= PROVIDER_TIMEOUT + Duration::from_millis(5)
        };
        let started = Instant::now();
        assert!(unavailable(replica.seal(b"secret", b"ad").await));
        assert!(within_timeout(started), "{:?}", started.elapsed());

        let started = Instant::now();
        assert!(unavailable(replica.unseal(&envelope, b"ad").await));
        assert!(within_timeout(started), "{:?}", started.elapsed());

        // Concurrent seals wait for one another, but none waits longer than the timeout.
        let started = Instant::now();
        let results = futures::future::join_all((0..4).map(|_| replica.seal(b"x", b"ad"))).await;
        assert!(results.into_iter().all(unavailable));
        assert!(within_timeout(started), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_rejected_wrapped_key_is_an_invalid_envelope() {
        let provider = MockProvider::new();
        let envelope = sealer(&provider).seal(b"secret", b"ad").await.unwrap();
        provider.set(REJECTS);
        assert!(invalid(sealer(&provider).unseal(&envelope, b"ad").await));
    }

    #[tokio::test]
    async fn unwrapped_keys_are_cached_and_unwrapped_once() {
        let provider = MockProvider::new();
        let sealer_a = sealer(&provider);
        let envelope = sealer_a.seal(b"secret", b"ad").await.unwrap();
        // The sealing replica already holds its own key.
        assert_eq!(sealer_a.unseal(&envelope, b"ad").await.unwrap(), b"secret");
        assert_eq!(provider.decrypts(), 0);

        let replica = sealer(&provider);
        let opened =
            futures::future::join_all((0..8).map(|_| replica.unseal(&envelope, b"ad"))).await;
        assert!(opened
            .iter()
            .all(|r| r.as_deref().ok() == Some(&b"secret"[..])));
        assert_eq!(provider.decrypts(), 1);
        assert_eq!(replica.unseal(&envelope, b"ad").await.unwrap(), b"secret");
        assert_eq!(provider.decrypts(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rotates_once_per_period_and_old_envelopes_still_open() {
        let provider = MockProvider::new();
        let sealer = sealer(&provider);

        let first = futures::future::join_all((0..16).map(|_| sealer.seal(b"one", b"ad"))).await;
        assert_eq!(provider.encrypts(), 1);
        let first: Vec<_> = first.into_iter().map(Result::unwrap).collect();
        assert!(first.iter().all(|e| split(e).0 == split(&first[0]).0));

        tokio::time::advance(ROTATION_PERIOD - Duration::from_secs(1)).await;
        let same = sealer.seal(b"one", b"ad").await.unwrap();
        assert_eq!(split(&same).0, split(&first[0]).0);
        assert_eq!(provider.encrypts(), 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        let rotated = futures::future::join_all((0..16).map(|_| sealer.seal(b"two", b"ad"))).await;
        assert_eq!(provider.encrypts(), 2, "one wrap per rotation");
        let rotated: Vec<_> = rotated.into_iter().map(Result::unwrap).collect();
        let after = sealer.seal(b"two", b"ad").await.unwrap();
        let new_wrapped = split(&after).0;
        assert_ne!(new_wrapped, split(&first[0]).0);
        // While the wrap is in flight, the other seals use the outgoing key.
        assert!(rotated
            .iter()
            .all(|e| [&new_wrapped, &split(&first[0]).0].contains(&&split(e).0)));
        assert!(rotated.iter().any(|e| split(e).0 == new_wrapped));
        assert_eq!(provider.encrypts(), 2);

        for envelope in first.iter().chain(&rotated) {
            assert!(sealer.unseal(envelope, b"ad").await.is_ok());
            assert!(RiseSealer::new(provider.clone())
                .unseal(envelope, b"ad")
                .await
                .is_ok());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_rotation_keeps_sealing_under_the_outgoing_key() {
        let provider = MockProvider::new();
        let sealer = sealer(&provider);
        let first = sealer.seal(b"one", b"ad").await.unwrap();

        tokio::time::advance(ROTATION_PERIOD).await;
        provider.set(FAILS);
        let during = sealer.seal(b"two", b"ad").await.unwrap();
        assert_eq!(split(&during).0, split(&first).0);
        assert_eq!(provider.encrypts(), 2);
        // No retry before the back-off has passed.
        sealer.seal(b"two", b"ad").await.unwrap();
        assert_eq!(provider.encrypts(), 2);

        provider.set(WORKS);
        tokio::time::advance(ROTATION_RETRY).await;
        let after = sealer.seal(b"three", b"ad").await.unwrap();
        assert_ne!(split(&after).0, split(&first).0);
        assert_eq!(provider.encrypts(), 3);
        assert_eq!(sealer.unseal(&during, b"ad").await.unwrap(), b"two");
    }

    #[tokio::test]
    async fn large_plaintexts_round_trip_through_the_local_provider() {
        let provider = Arc::new(LocalEncryptionProvider::new(&STANDARD.encode([9u8; 32])).unwrap());
        let sealer = RiseSealer::new(provider.clone());
        let mut plaintext = vec![0u8; 64 * 1024];
        rand::rng().fill_bytes(&mut plaintext);
        let envelope = sealer.seal(&plaintext, b"code").await.unwrap();
        assert!(envelope.len() < plaintext.len() * 4 / 3 + 200);
        assert_eq!(
            RiseSealer::new(provider)
                .unseal(&envelope, b"code")
                .await
                .unwrap(),
            plaintext
        );
    }
}
