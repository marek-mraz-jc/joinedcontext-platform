//! SOPS + age decryption of the repository's secret files (CC-06, OPS-37, ADR-N-012).
//!
//! A manifest never carries a secret, only a `secretRef` naming one. The values live in
//! `*.enc.yaml` files encrypted with SOPS to one or more age recipients, and the private
//! age identity is mounted next to the reconciler, never committed. This module unwraps a
//! file's data key with that identity and decrypts the values into memory at apply time.
//!
//! Plaintext leaves the store only through [`SecretValue::expose`]. It is never written
//! back to a file, never rendered by `Debug`, and never part of an error message; the
//! buffers holding it are wiped when the store is dropped.
//!
//! The file layout is one level of names over their keys, which is what a `secretRef`
//! addresses:
//!
//! ```yaml
//! parking-mqtt-creds:
//!     username: ENC[AES256_GCM,data:…]
//!     password: ENC[AES256_GCM,data:…]
//! portal-session-key: ENC[AES256_GCM,data:…]
//! ```
//!
//! Every leaf must be encrypted. A secrets file with a plaintext value is refused rather
//! than half-read, because a plaintext value in a repository is exactly what CC-06 forbids.

use crate::loader::LoadError;
use aes_gcm::aead::{consts::U32, Aead, Payload};
use aes_gcm::KeyInit;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use jc_core::SecretRef;
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use zeroize::Zeroizing;

/// File names the reconciler treats as encrypted secret files, everywhere in the tree.
///
/// The loader skips exactly these, so an encrypted file is never parsed as a manifest.
pub const ENCRYPTED_SUFFIXES: [&str; 2] = [".enc.yaml", ".enc.yml"];

/// SOPS's own key for its metadata block inside the encrypted file.
const METADATA_KEY: &str = "sops";

/// AES-256-GCM with the 32-byte nonce SOPS writes; the RustCrypto default is 12.
type SopsCipher = aes_gcm::AesGcm<aes_gcm::aes::Aes256, U32>;

const DATA_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 32;

/// Whether a file name is an encrypted secret file (CC-06).
pub fn is_encrypted_file(file_name: &str) -> bool {
    ENCRYPTED_SUFFIXES.iter().any(|s| file_name.ends_with(s))
}

/// A decrypted secret value.
///
/// It can be read only by asking for it explicitly. `Debug` prints a placeholder, there is
/// no `Display` and no `Serialize`, and the buffer is wiped on drop (CC-06, OPS-37).
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    /// Wraps a plaintext value the reconciler already holds in memory.
    ///
    /// Crate-visible on purpose: the callers are the two backends that decrypt or fetch, so
    /// no command can turn a string it read out of a manifest into a secret value.
    pub(crate) fn new(plaintext: String) -> Self {
        Self(Zeroizing::new(plaintext))
    }

    /// A secret the process environment carries: a token injected into a Job, or the
    /// variable `jcctl publish ckan --api-token-env` names (EP-67).
    ///
    /// `None` when the variable is unset or empty, which are the same thing to a caller
    /// that needs a credential; the value is moved into the wiped buffer, not copied.
    pub fn from_env(variable: &str) -> Option<Self> {
        std::env::var(variable)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(Self::new)
    }

    /// The plaintext. Every call site that uses this is a place where a secret can escape.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Length of the plaintext in bytes, for length checks that must not read the value.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the decrypted value is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(redacted)")
    }
}

/// Everything that can go wrong resolving a secret. No variant carries plaintext.
#[derive(Debug, thiserror::Error)]
pub enum SopsError {
    /// The repository could not be walked (CC-08).
    #[error(transparent)]
    Repository(#[from] LoadError),

    /// A secret file could not be read.
    #[error("I/O error reading {path}: {source}")]
    Io {
        /// Path the error happened on.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A secret file is not YAML, or not a mapping of names.
    #[error("{path} is not a SOPS secrets mapping: {message}")]
    Parse {
        /// Repository-relative path of the file.
        path: PathBuf,
        /// Parser message, never a value.
        message: String,
    },

    /// The file has no `sops:` metadata block, so it is not encrypted at all (CC-06).
    #[error("{path} has no SOPS metadata block: it is not an encrypted secrets file")]
    NotSopsEncrypted {
        /// Repository-relative path of the file.
        path: PathBuf,
    },

    /// The file is encrypted to key backends this reconciler does not implement.
    #[error("{path} has no age recipients: only SOPS files encrypted with age are supported")]
    NoAgeRecipients {
        /// Repository-relative path of the file.
        path: PathBuf,
    },

    /// None of the supplied identities is a recipient of the file.
    #[error("{path} cannot be decrypted: no supplied age identity is a recipient of it")]
    NoMatchingIdentity {
        /// Repository-relative path of the file.
        path: PathBuf,
    },

    /// An age key file held no usable identity.
    #[error("{path} contains no usable age identity: {reason}")]
    InvalidIdentity {
        /// Path of the key file.
        path: PathBuf,
        /// Why it was rejected, never the key material.
        reason: String,
    },

    /// A value in the file is not an `ENC[AES256_GCM,…]` literal (CC-06).
    #[error("{path}: `{field}` is not encrypted; every value in a secrets file must be")]
    NotEncrypted {
        /// Repository-relative path of the file.
        path: PathBuf,
        /// Dotted position of the value inside the file.
        field: String,
    },

    /// A value could not be decrypted, or the file has been tampered with.
    #[error("{path}: `{field}` could not be decrypted ({reason})")]
    Value {
        /// Repository-relative path of the file.
        path: PathBuf,
        /// Dotted position of the value inside the file.
        field: String,
        /// What failed, never the value.
        reason: &'static str,
    },

    /// The file's contents do not match its message authentication code.
    #[error(
        "{path} failed its SOPS integrity check: the file has been modified since it was encrypted"
    )]
    MacMismatch {
        /// Repository-relative path of the file.
        path: PathBuf,
    },

    /// A secrets file nests deeper than name and key, or holds a list.
    #[error("{path}: `{field}` must be a value or a mapping of keys, nothing deeper")]
    UnsupportedShape {
        /// Repository-relative path of the file.
        path: PathBuf,
        /// Dotted position inside the file.
        field: String,
    },

    /// Two files declare the same secret name.
    #[error("duplicate secret `{name}`: first declared in {first}, again in {second}")]
    DuplicateSecret {
        /// The name both files declare.
        name: String,
        /// Where it was seen first.
        first: PathBuf,
        /// Where it was seen again.
        second: PathBuf,
    },

    /// A manifest references a secret no file declares.
    #[error("secret `{name}` is not declared in any encrypted secrets file")]
    SecretNotFound {
        /// The referenced name.
        name: String,
    },

    /// A manifest references a key the secret does not have.
    #[error("secret `{name}` has no key `{key}`")]
    KeyNotFound {
        /// The referenced name.
        name: String,
        /// The referenced key.
        key: String,
    },

    /// A multi-key secret was referenced without saying which key.
    #[error("secret `{name}` has several keys; the reference must name one")]
    KeyRequired {
        /// The referenced name.
        name: String,
    },

    /// A single-value secret was referenced with a key.
    #[error("secret `{name}` is a single value and has no key `{key}`")]
    NotKeyed {
        /// The referenced name.
        name: String,
        /// The key the reference asked for.
        key: String,
    },
}

/// One secret: either a single value or a set of named keys.
#[derive(Debug)]
enum Value {
    Single(SecretValue),
    Keyed(BTreeMap<String, SecretValue>),
}

#[derive(Debug)]
struct Secret {
    source: PathBuf,
    value: Value,
}

/// The repository's decrypted secrets, indexed by the name manifests reference (CC-06).
#[derive(Debug, Default)]
pub struct SecretStore {
    secrets: BTreeMap<String, Secret>,
}

impl SecretStore {
    /// Decrypts every `*.enc.yaml` under `root` (CC-06, OPS-37).
    ///
    /// The whole repository is read at once because a reference does not say which file
    /// holds it. Two files declaring the same name is an error, not a silent last-wins.
    pub fn load_dir(root: &Path, identities: &[age::x25519::Identity]) -> Result<Self, SopsError> {
        let mut store = Self::default();
        for entry in crate::loader::walk_files(root)? {
            let name = match entry.file_name().to_str() {
                Some(n) if is_encrypted_file(n) => n,
                _ => continue,
            };
            debug_assert!(!name.is_empty());
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_path_buf();
            store.read_file(entry.path(), rel, identities)?;
        }
        Ok(store)
    }

    /// Decrypts a single secrets file (CC-06).
    pub fn load_file(path: &Path, identities: &[age::x25519::Identity]) -> Result<Self, SopsError> {
        let mut store = Self::default();
        store.read_file(path, path.to_path_buf(), identities)?;
        Ok(store)
    }

    /// Resolves what a manifest references (CC-06).
    pub fn resolve(&self, reference: &SecretRef) -> Result<&SecretValue, SopsError> {
        let secret =
            self.secrets
                .get(&reference.name)
                .ok_or_else(|| SopsError::SecretNotFound {
                    name: reference.name.clone(),
                })?;

        match (&secret.value, reference.key.as_deref()) {
            (Value::Single(v), None) => Ok(v),
            (Value::Single(_), Some(key)) => Err(SopsError::NotKeyed {
                name: reference.name.clone(),
                key: key.to_owned(),
            }),
            (Value::Keyed(keys), Some(key)) => {
                keys.get(key).ok_or_else(|| SopsError::KeyNotFound {
                    name: reference.name.clone(),
                    key: key.to_owned(),
                })
            }
            (Value::Keyed(keys), None) => match keys.iter().next() {
                Some((_, v)) if keys.len() == 1 => Ok(v),
                _ => Err(SopsError::KeyRequired {
                    name: reference.name.clone(),
                }),
            },
        }
    }

    /// The declared secret names, sorted. Names are not secret; values are.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.secrets.keys().map(String::as_str)
    }

    /// Which file a secret came from, for diagnostics.
    pub fn source_of(&self, name: &str) -> Option<&Path> {
        self.secrets.get(name).map(|s| s.source.as_path())
    }

    /// How many secrets are loaded.
    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    /// Whether no secret is loaded.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    fn read_file(
        &mut self,
        path: &Path,
        rel: PathBuf,
        identities: &[age::x25519::Identity],
    ) -> Result<(), SopsError> {
        let text =
            Zeroizing::new(
                std::fs::read_to_string(path).map_err(|source| SopsError::Io {
                    path: rel.clone(),
                    source,
                })?,
            );
        let secrets = decrypt_document(&rel, &text, identities)?;
        for (name, value) in secrets {
            if let Some(existing) = self.secrets.get(&name) {
                return Err(SopsError::DuplicateSecret {
                    name,
                    first: existing.source.clone(),
                    second: rel,
                });
            }
            self.secrets.insert(
                name,
                Secret {
                    source: rel.clone(),
                    value,
                },
            );
        }
        Ok(())
    }
}

/// Reads age identities from a key file in `age-keygen` / `SOPS_AGE_KEY_FILE` format.
///
/// Comment lines carry the public key and creation date; only the secret lines matter. The
/// file content is wiped after parsing, and no error message repeats a line from it.
pub fn identities_from_file(path: &Path) -> Result<Vec<age::x25519::Identity>, SopsError> {
    let text = Zeroizing::new(
        std::fs::read_to_string(path).map_err(|source| SopsError::Io {
            path: path.to_path_buf(),
            source,
        })?,
    );

    let mut identities = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let identity =
            age::x25519::Identity::from_str(line).map_err(|reason| SopsError::InvalidIdentity {
                path: path.to_path_buf(),
                reason: reason.to_owned(),
            })?;
        identities.push(identity);
    }

    if identities.is_empty() {
        return Err(SopsError::InvalidIdentity {
            path: path.to_path_buf(),
            reason: "the file has no key line".to_owned(),
        });
    }
    Ok(identities)
}

/// SOPS's metadata block. Unknown members are ignored: it is a foreign format that grows,
/// and the members that would change the meaning of the file are refused elsewhere (a key
/// backend other than age leaves `age` empty, and any partially encrypted file is refused
/// because one of its values is plaintext).
#[derive(Debug, Deserialize)]
struct Metadata {
    #[serde(default)]
    age: Vec<AgeStanza>,
    lastmodified: String,
    mac: String,
}

#[derive(Debug, Deserialize)]
struct AgeStanza {
    enc: String,
}

fn decrypt_document(
    path: &Path,
    text: &str,
    identities: &[age::x25519::Identity],
) -> Result<Vec<(String, Value)>, SopsError> {
    let mut document: serde_norway::Mapping =
        serde_norway::from_str(text).map_err(|e| SopsError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

    let raw_metadata =
        document
            .remove(METADATA_KEY)
            .ok_or_else(|| SopsError::NotSopsEncrypted {
                path: path.to_path_buf(),
            })?;
    let metadata: Metadata =
        serde_norway::from_value(raw_metadata).map_err(|e| SopsError::Parse {
            path: path.to_path_buf(),
            message: format!("`{METADATA_KEY}` is not SOPS metadata: {e}"),
        })?;

    let data_key = unwrap_data_key(path, &metadata, identities)?;

    // One pass: decrypt every value in file order and feed the plaintext to the file's
    // message authentication code, which is the concatenation SOPS hashed when it wrote it.
    let mut mac = Sha512::new();
    let mut secrets = Vec::with_capacity(document.len());

    for (name, node) in document {
        let name = match name {
            serde_norway::Value::String(s) => s,
            other => {
                return Err(SopsError::UnsupportedShape {
                    path: path.to_path_buf(),
                    field: format!("{other:?}"),
                })
            }
        };

        let value = match node {
            serde_norway::Value::String(literal) => {
                let plaintext =
                    decrypt_leaf(path, &name, &format!("{name}:"), &literal, &data_key)?;
                mac.update(plaintext.expose().as_bytes());
                Value::Single(plaintext)
            }
            serde_norway::Value::Mapping(keys) => {
                let mut resolved = BTreeMap::new();
                for (key, node) in keys {
                    let key = match key {
                        serde_norway::Value::String(s) => s,
                        other => {
                            return Err(SopsError::UnsupportedShape {
                                path: path.to_path_buf(),
                                field: format!("{name}.{other:?}"),
                            })
                        }
                    };
                    let field = format!("{name}.{key}");
                    let literal = match node {
                        serde_norway::Value::String(s) => s,
                        _ => {
                            return Err(SopsError::UnsupportedShape {
                                path: path.to_path_buf(),
                                field,
                            })
                        }
                    };
                    let plaintext =
                        decrypt_leaf(path, &field, &format!("{name}:{key}:"), &literal, &data_key)?;
                    mac.update(plaintext.expose().as_bytes());
                    resolved.insert(key, plaintext);
                }
                Value::Keyed(resolved)
            }
            _ => {
                return Err(SopsError::UnsupportedShape {
                    path: path.to_path_buf(),
                    field: name,
                })
            }
        };

        secrets.push((name, value));
    }

    let computed = hex_upper(&mac.finalize());
    let stored = decrypt_leaf(
        path,
        "sops.mac",
        &metadata.lastmodified,
        &metadata.mac,
        &data_key,
    )?;
    if computed != stored.expose() {
        return Err(SopsError::MacMismatch {
            path: path.to_path_buf(),
        });
    }

    Ok(secrets)
}

fn unwrap_data_key(
    path: &Path,
    metadata: &Metadata,
    identities: &[age::x25519::Identity],
) -> Result<Zeroizing<Vec<u8>>, SopsError> {
    if metadata.age.is_empty() {
        return Err(SopsError::NoAgeRecipients {
            path: path.to_path_buf(),
        });
    }

    for stanza in &metadata.age {
        let armored = age::armor::ArmoredReader::new(stanza.enc.as_bytes());
        let decryptor = match age::Decryptor::new(armored) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let mut reader = match decryptor.decrypt(identities.iter().map(|i| i as &dyn age::Identity))
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut key = Zeroizing::new(Vec::with_capacity(DATA_KEY_LEN));
        if reader.read_to_end(&mut key).is_err() || key.len() != DATA_KEY_LEN {
            continue;
        }
        return Ok(key);
    }

    Err(SopsError::NoMatchingIdentity {
        path: path.to_path_buf(),
    })
}

fn decrypt_leaf(
    path: &Path,
    field: &str,
    aad: &str,
    literal: &str,
    data_key: &[u8],
) -> Result<SecretValue, SopsError> {
    let parts = parse_literal(literal).ok_or_else(|| SopsError::NotEncrypted {
        path: path.to_path_buf(),
        field: field.to_owned(),
    })?;

    decrypt_parts(&parts, aad, data_key).map_err(|reason| SopsError::Value {
        path: path.to_path_buf(),
        field: field.to_owned(),
        reason,
    })
}

/// The three base64 members of one `ENC[AES256_GCM,data:…,iv:…,tag:…,type:…]` literal.
///
/// `type` is deliberately dropped: SOPS stores the YAML type it took the value from, but a
/// secret is injected as text wherever it goes, and the bytes it hashed for the file's MAC
/// are the plaintext ones, whatever the type says.
struct Parts<'a> {
    data: &'a str,
    iv: &'a str,
    tag: &'a str,
}

fn parse_literal(literal: &str) -> Option<Parts<'_>> {
    let body = literal.strip_prefix("ENC[AES256_GCM,")?.strip_suffix(']')?;

    let (mut data, mut iv, mut tag) = (None, None, None);
    for member in body.split(',') {
        let (name, value) = member.split_once(':')?;
        match name {
            "data" => data = Some(value),
            "iv" => iv = Some(value),
            "tag" => tag = Some(value),
            "type" => {}
            _ => return None,
        }
    }

    Some(Parts {
        data: data?,
        iv: iv?,
        tag: tag?,
    })
}

fn decrypt_parts(
    parts: &Parts<'_>,
    aad: &str,
    data_key: &[u8],
) -> Result<SecretValue, &'static str> {
    if data_key.len() != DATA_KEY_LEN {
        return Err("the file's data key is not 32 bytes");
    }
    let nonce = BASE64
        .decode(parts.iv)
        .map_err(|_| "the iv is not base64")?;
    if nonce.len() != NONCE_LEN {
        return Err("the iv is not 32 bytes");
    }
    let mut sealed = BASE64
        .decode(parts.data)
        .map_err(|_| "the ciphertext is not base64")?;
    let mut tag = BASE64
        .decode(parts.tag)
        .map_err(|_| "the tag is not base64")?;
    sealed.append(&mut tag);

    let cipher = SopsCipher::new(aes_gcm::Key::<SopsCipher>::from_slice(data_key));
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                aes_gcm::Nonce::<U32>::from_slice(&nonce),
                Payload {
                    msg: &sealed,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| "authentication failed")?,
    );

    let text = std::str::from_utf8(&plaintext).map_err(|_| "the plaintext is not UTF-8")?;
    Ok(SecretValue::new(text.to_owned()))
}

fn hex_upper(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02X}");
    }
    out
}
