//! Reticulum-signed release files: `.rsg` (one artifact) and `.rsm`
//! (the manifest). Both are a 64-byte Ed25519 signature followed by a
//! MessagePack map; the signature covers those MessagePack bytes.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rmpv::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::wire::{self, MapRef};

/// Something that signs with a Reticulum identity.
pub trait Signer {
    /// The 16-byte identity hash.
    fn identity_hash(&self) -> [u8; 16];
    /// The 64-byte public key (X25519 key then Ed25519 key).
    fn public_key(&self) -> [u8; 64];
    /// Ed25519 signature over `message`.
    fn sign(&self, message: &[u8]) -> Result<[u8; 64]>;
}

/// The identity hash of a public key: the first 16 bytes of its SHA-256.
pub fn identity_hash(public_key: &[u8; 64]) -> [u8; 16] {
    let digest = Sha256::digest(public_key);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Check an Ed25519 signature made by a Reticulum identity's public key.
pub fn verify_signature(public_key: &[u8; 64], message: &[u8], signature: &[u8]) -> Result<()> {
    let key: [u8; 32] = public_key[32..].try_into().map_err(|_| Error::Verify("bad public key".into()))?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| Error::Verify("bad public key".into()))?;
    let signature = Signature::from_slice(signature).map_err(|_| Error::Verify("bad signature length".into()))?;
    key.verify(message, &signature).map_err(|_| Error::Verify("signature does not match".into()))
}

/// Who signed a file and when, after the signature and key checks passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerInfo {
    pub signer: [u8; 16],
    pub public_key: [u8; 64],
    pub timestamp: u64,
}

fn split_signed(file: &[u8]) -> Result<(&[u8], &[u8])> {
    if file.len() <= 64 {
        return Err(Error::Verify("file is too short to be signed".into()));
    }
    Ok(file.split_at(64))
}

/// Read and check the `meta` block's signer fields against the signature.
fn check_signer(file: &[u8]) -> Result<(Value, SignerInfo)> {
    let (signature, body) = split_signed(file)?;
    let value = wire::decode(body).map_err(|e| Error::Verify(e.to_string()))?;
    let map = MapRef::new(&value).map_err(|e| Error::Verify(e.to_string()))?;
    let meta = map.map("meta").ok_or_else(|| Error::Verify("missing meta".into()))?;
    let public_key: [u8; 64] =
        meta.bytes("pubkey").and_then(|b| b.try_into().ok()).ok_or_else(|| Error::Verify("missing or short pubkey".into()))?;
    let signer: [u8; 16] = meta.bytes("signer").and_then(|b| b.try_into().ok()).ok_or_else(|| Error::Verify("missing signer".into()))?;
    if identity_hash(&public_key) != signer {
        return Err(Error::Verify("signer does not match the public key".into()));
    }
    verify_signature(&public_key, body, signature)?;
    let timestamp = meta.u64("timestamp").unwrap_or(0);
    Ok((value, SignerInfo { signer, public_key, timestamp }))
}

/// Verify a `.rsg` file against the artifact it signs.
pub fn verify_rsg(artifact: &[u8], rsg: &[u8]) -> Result<SignerInfo> {
    let (value, info) = check_signer(rsg)?;
    let map = MapRef::new(&value).map_err(|e| Error::Verify(e.to_string()))?;
    if map.str("hashtype").as_deref() != Some("sha256") {
        return Err(Error::Verify("unsupported hash type".into()));
    }
    let expected = map.bytes("hash").ok_or_else(|| Error::Verify("missing hash".into()))?;
    if expected != Sha256::digest(artifact).as_slice() {
        return Err(Error::Verify("artifact does not match its signed hash".into()));
    }
    Ok(info)
}

/// The parts of a manifest a caller shows or checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub signer: SignerInfo,
    pub name: String,
    pub version: String,
    pub released: String,
    pub origin: Option<[u8; 16]>,
    pub path: String,
    pub commit: String,
    pub message: Vec<u8>,
    pub artifacts: Vec<ManifestArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestArtifact {
    pub name: String,
    pub rsg: Vec<u8>,
}

/// Verify a `.rsm` file's signature, its signer and its `hash` (the SHA-256
/// of the release notes), then read it. Each listed artifact's `.rsg` is
/// checked with [`verify_release_files`].
pub fn verify_manifest(rsm: &[u8]) -> Result<Manifest> {
    let (value, signer) = check_signer(rsm)?;
    let map = MapRef::new(&value).map_err(|e| Error::Verify(e.to_string()))?;
    let meta = map.map("meta").ok_or_else(|| Error::Verify("missing meta".into()))?;
    let message = map.bytes("message").unwrap_or_default();
    if map.bytes("hash") != Some(Sha256::digest(message).as_slice()) {
        return Err(Error::Verify("release notes do not match the manifest hash".into()));
    }
    let artifacts = meta
        .array("artifacts")
        .iter()
        .filter_map(|entry| {
            let entry = MapRef::new(entry).ok()?;
            Some(ManifestArtifact { name: entry.str("name")?, rsg: entry.bytes("rsg")?.to_vec() })
        })
        .collect();
    Ok(Manifest {
        signer,
        name: meta.str("name").unwrap_or_default(),
        version: meta.str("version").unwrap_or_default(),
        released: meta.str("released").unwrap_or_default(),
        origin: meta.bytes("origin").and_then(|b| b.try_into().ok()),
        path: meta.str("path").unwrap_or_default(),
        commit: meta.str("commit").unwrap_or_default(),
        message: message.to_vec(),
        artifacts,
    })
}

/// Verify a whole release: the manifest, and for each artifact it lists the
/// `.rsg` signature, the artifact's hash, and that the same identity signed
/// both. `files` maps artifact names to their contents.
pub fn verify_release_files(rsm: &[u8], files: &[(String, Vec<u8>)]) -> Result<Manifest> {
    let manifest = verify_manifest(rsm)?;
    for artifact in &manifest.artifacts {
        let (_, data) = files
            .iter()
            .find(|(name, _)| *name == artifact.name)
            .ok_or_else(|| Error::Verify(format!("missing artifact {}", artifact.name)))?;
        let info = verify_rsg(data, &artifact.rsg)?;
        if info.signer != manifest.signer.signer {
            return Err(Error::Verify(format!("{} was signed by a different identity than the manifest", artifact.name)));
        }
    }
    Ok(manifest)
}

fn sign_map(signer: &dyn Signer, body: Value) -> Result<Vec<u8>> {
    let mut packed = Vec::new();
    rmpv::encode::write_value(&mut packed, &body).map_err(|e| Error::InvalidInput(e.to_string()))?;
    let signature = signer.sign(&packed)?;
    let mut out = signature.to_vec();
    out.extend_from_slice(&packed);
    Ok(out)
}

fn meta_base(signer: &dyn Signer, timestamp: u64) -> Vec<(Value, Value)> {
    vec![
        (Value::from("signer"), Value::Binary(signer.identity_hash().to_vec())),
        (Value::from("pubkey"), Value::Binary(signer.public_key().to_vec())),
        (Value::from("timestamp"), Value::from(timestamp)),
    ]
}

/// Make the `.rsg` file for one artifact.
pub fn make_rsg(signer: &dyn Signer, artifact: &[u8], timestamp: u64) -> Result<Vec<u8>> {
    let body = Value::Map(vec![
        (Value::from("hashtype"), Value::from("sha256")),
        (Value::from("hash"), Value::Binary(Sha256::digest(artifact).to_vec())),
        (Value::from("meta"), Value::Map(meta_base(signer, timestamp))),
    ]);
    sign_map(signer, body)
}

/// What a caller gives to build a manifest.
#[derive(Debug, Clone)]
pub struct ManifestInput<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub released: &'a str,
    pub origin: [u8; 16],
    pub path: &'a str,
    pub commit: &'a str,
    pub message: &'a [u8],
    /// Artifact names with their `.rsg` file contents.
    pub artifacts: &'a [(String, Vec<u8>)],
}

/// Make the `.rsm` manifest. Its `hash` field is the SHA-256 of the release
/// notes, which is what a manifest from stock rngit was observed to hold.
pub fn make_rsm(signer: &dyn Signer, input: &ManifestInput<'_>, timestamp: u64) -> Result<Vec<u8>> {
    let mut meta = meta_base(signer, timestamp);
    meta.extend([
        (Value::from("name"), Value::from(input.name)),
        (Value::from("version"), Value::from(input.version)),
        (Value::from("released"), Value::from(input.released)),
        (Value::from("origin"), Value::Binary(input.origin.to_vec())),
        (Value::from("path"), Value::from(input.path)),
        (Value::from("commit"), Value::from(input.commit)),
        (
            Value::from("artifacts"),
            Value::Array(
                input
                    .artifacts
                    .iter()
                    .map(|(name, rsg)| {
                        Value::Map(vec![
                            (Value::from("name"), Value::from(name.as_str())),
                            (Value::from("rsg"), Value::Binary(rsg.clone())),
                        ])
                    })
                    .collect(),
            ),
        ),
    ]);
    let body = Value::Map(vec![
        (Value::from("hashtype"), Value::from("sha256")),
        (Value::from("hash"), Value::Binary(Sha256::digest(input.message).to_vec())),
        (Value::from("meta"), Value::Map(meta)),
        (Value::from("message"), Value::Binary(input.message.to_vec())),
    ]);
    sign_map(signer, body)
}

#[cfg(test)]
pub(crate) mod test_signer {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    /// A throwaway identity for tests.
    pub struct TestSigner {
        key: SigningKey,
        public: [u8; 64],
    }

    impl TestSigner {
        pub fn new(seed: u8) -> Self {
            let key = SigningKey::from_bytes(&[seed; 32]);
            let mut public = [seed; 64];
            public[32..].copy_from_slice(key.verifying_key().as_bytes());
            TestSigner { key, public }
        }
    }

    impl Signer for TestSigner {
        fn identity_hash(&self) -> [u8; 16] {
            identity_hash(&self.public)
        }
        fn public_key(&self) -> [u8; 64] {
            self.public
        }
        fn sign(&self, message: &[u8]) -> Result<[u8; 64]> {
            Ok(self.key.sign(message).to_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_signer::TestSigner;
    use super::*;

    const CAPTURED_MANIFEST: &[u8] = include_bytes!("../tests/fixtures/release-manifest.rsm");

    #[test]
    fn verifies_manifest_captured_from_stock_rngit() {
        let manifest = verify_manifest(CAPTURED_MANIFEST).unwrap();
        assert_eq!(manifest.name, "demo.git");
        assert_eq!(manifest.path, "public/demo.git");
        assert!(!manifest.artifacts.is_empty());
        for artifact in &manifest.artifacts {
            // Each embedded .rsg is itself a signed file from the same identity.
            let (_, info) = check_signer(&artifact.rsg).unwrap();
            assert_eq!(info.signer, manifest.signer.signer);
        }
    }

    #[test]
    fn round_trips_our_own_files() {
        let signer = TestSigner::new(7);
        let data = b"artifact bytes".to_vec();
        let rsg = make_rsg(&signer, &data, 1000).unwrap();
        let info = verify_rsg(&data, &rsg).unwrap();
        assert_eq!(info.signer, signer.identity_hash());
        assert_eq!(info.timestamp, 1000);

        let artifacts = [("a.bin".to_string(), rsg)];
        let rsm = make_rsm(
            &signer,
            &ManifestInput {
                name: "demo.git",
                version: "v1",
                released: "2026-10-26T00:00:00Z",
                origin: [9; 16],
                path: "public/demo.git",
                commit: "abc",
                message: b"notes",
                artifacts: &artifacts,
            },
            1001,
        )
        .unwrap();
        let manifest = verify_release_files(&rsm, &[("a.bin".to_string(), data)]).unwrap();
        assert_eq!(manifest.version, "v1");
        assert_eq!(manifest.message, b"notes");
        assert_eq!(manifest.origin, Some([9; 16]));
    }

    #[test]
    fn rejects_tampering() {
        let signer = TestSigner::new(7);
        let data = b"artifact bytes".to_vec();
        let rsg = make_rsg(&signer, &data, 1).unwrap();
        assert!(verify_rsg(b"other bytes", &rsg).is_err());

        let mut flipped = rsg.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(verify_rsg(&data, &flipped).is_err());

        let mut bad_sig = rsg.clone();
        bad_sig[0] ^= 1;
        assert!(verify_rsg(&data, &bad_sig).is_err());

        assert!(verify_rsg(&data, &rsg[..10]).is_err());
        assert!(verify_manifest(&[0u8; 100]).is_err());
    }

    #[test]
    fn rejects_manifest_whose_notes_were_swapped() {
        let signer = TestSigner::new(4);
        let input = ManifestInput {
            name: "n",
            version: "v",
            released: "r",
            origin: [0; 16],
            path: "g/n.git",
            commit: "c",
            message: b"honest notes",
            artifacts: &[],
        };
        let rsm = make_rsm(&signer, &input, 1).unwrap();
        assert!(verify_manifest(&rsm).is_ok());
        // Same hash, different notes, re-signed by the same key.
        let body = wire::decode(&rsm[64..]).unwrap();
        let Value::Map(mut entries) = body else { panic!() };
        for (k, v) in entries.iter_mut() {
            if k.as_str() == Some("message") {
                *v = Value::Binary(b"swapped notes".to_vec());
            }
        }
        let forged = sign_map(&signer, Value::Map(entries)).unwrap();
        assert!(verify_manifest(&forged).is_err());
    }

    #[test]
    fn rejects_artifact_signed_by_another_identity() {
        let a = TestSigner::new(1);
        let b = TestSigner::new(2);
        let data = b"x".to_vec();
        let rsg_b = make_rsg(&b, &data, 1).unwrap();
        let artifacts = [("x".to_string(), rsg_b)];
        let rsm = make_rsm(
            &a,
            &ManifestInput {
                name: "n",
                version: "v",
                released: "r",
                origin: [0; 16],
                path: "g/n.git",
                commit: "c",
                message: b"",
                artifacts: &artifacts,
            },
            1,
        )
        .unwrap();
        assert!(verify_release_files(&rsm, &[("x".to_string(), data)]).is_err());
    }

    #[test]
    fn rejects_missing_artifact() {
        let signer = TestSigner::new(3);
        let rsg = make_rsg(&signer, b"x", 1).unwrap();
        let artifacts = [("x".to_string(), rsg)];
        let rsm = make_rsm(
            &signer,
            &ManifestInput {
                name: "n",
                version: "v",
                released: "r",
                origin: [0; 16],
                path: "g/n.git",
                commit: "c",
                message: b"",
                artifacts: &artifacts,
            },
            1,
        )
        .unwrap();
        assert!(verify_release_files(&rsm, &[]).is_err());
    }

    #[test]
    fn rejects_signer_that_is_not_the_key_hash() {
        let signer = TestSigner::new(5);
        let mut rsg = make_rsg(&signer, b"x", 1).unwrap();
        // Corrupt the signer field inside the body and re-sign it: the hash check must catch it.
        let body = rsg.split_off(64);
        let mut value = wire::decode(&body).unwrap();
        if let Value::Map(entries) = &mut value {
            for (k, v) in entries.iter_mut() {
                if k.as_str() == Some("meta")
                    && let Value::Map(meta) = v
                {
                    for (mk, mv) in meta.iter_mut() {
                        if mk.as_str() == Some("signer") {
                            *mv = Value::Binary(vec![0; 16]);
                        }
                    }
                }
            }
        }
        let forged = sign_map(&signer, value).unwrap();
        assert!(verify_rsg(b"x", &forged).is_err());
    }
}
