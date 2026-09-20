//! SOPS + age secret resolution (T-0136, CC-06, OPS-37, ADR-N-012).
//!
//! The fixtures are written here rather than committed, because a committed encrypted
//! fixture needs its private key committed next to it to be readable, and a repository
//! that carries an age private key is the thing this module exists to prevent. Every test
//! therefore generates its own keypair and encrypts exactly the way SOPS does: a random
//! data key wrapped for the age recipient, AES-256-GCM per value with a 32-byte nonce and
//! the value's path as additional data, and a SHA-512 message authentication code over the
//! plaintexts, itself encrypted under the file's timestamp.

use aes_gcm::aead::{consts::U32, Aead, Payload};
use aes_gcm::KeyInit;
use age::secrecy::ExposeSecret as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use jc_core::SecretRef;
use jcctl::loader::Repository;
use jcctl::secrets::sops::{identities_from_file, SecretStore, SopsError};
use sha2::{Digest, Sha512};
use std::path::{Path, PathBuf};

type SopsCipher = aes_gcm::AesGcm<aes_gcm::aes::Aes256, U32>;

const DATA_KEY: [u8; 32] = [7u8; 32];
const LAST_MODIFIED: &str = "2026-09-06T12:00:00Z";

/// What a secrets file holds under one name.
enum Node {
    Value(&'static str),
    Keys(&'static [(&'static str, &'static str)]),
}

/// Encrypts one value the way SOPS does: the path of the value is the additional data, so
/// a ciphertext moved to another field no longer authenticates.
fn seal(plaintext: &str, aad: &str, nonce_seed: u8) -> String {
    let cipher = SopsCipher::new(aes_gcm::Key::<SopsCipher>::from_slice(&DATA_KEY));
    let nonce = [nonce_seed; 32];
    let sealed = cipher
        .encrypt(
            aes_gcm::Nonce::<U32>::from_slice(&nonce),
            Payload {
                msg: plaintext.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .expect("encrypt");
    let (data, tag) = sealed.split_at(sealed.len() - 16);
    format!(
        "ENC[AES256_GCM,data:{},iv:{},tag:{},type:str]",
        BASE64.encode(data),
        BASE64.encode(nonce),
        BASE64.encode(tag),
    )
}

/// Writes a SOPS file for `recipient` holding `entries`, in the order given.
fn sops_file(entries: &[(&str, Node)], recipient: &age::x25519::Recipient) -> String {
    let mut yaml = String::new();
    let mut mac = Sha512::new();
    let mut seed = 1u8;

    for (name, node) in entries {
        match node {
            Node::Value(value) => {
                mac.update(value.as_bytes());
                yaml.push_str(&format!(
                    "{name}: {}\n",
                    seal(value, &format!("{name}:"), seed)
                ));
                seed += 1;
            }
            Node::Keys(keys) => {
                yaml.push_str(&format!("{name}:\n"));
                for (key, value) in *keys {
                    mac.update(value.as_bytes());
                    let literal = seal(value, &format!("{name}:{key}:"), seed);
                    yaml.push_str(&format!("    {key}: {literal}\n"));
                    seed += 1;
                }
            }
        }
    }

    let mut digest = String::new();
    for byte in mac.finalize() {
        digest.push_str(&format!("{byte:02X}"));
    }

    let armored = age::encrypt_and_armor(recipient, &DATA_KEY).expect("wrap data key");
    let indented: String = armored
        .lines()
        .map(|line| format!("            {line}\n"))
        .collect();

    yaml.push_str("sops:\n    age:\n");
    yaml.push_str(&format!(
        "        - recipient: {recipient}\n          enc: |\n{indented}"
    ));
    yaml.push_str(&format!("    lastmodified: \"{LAST_MODIFIED}\"\n"));
    yaml.push_str(&format!("    mac: {}\n", seal(&digest, LAST_MODIFIED, 200)));
    yaml.push_str("    unencrypted_suffix: _unencrypted\n    version: 3.10.2\n");
    yaml
}

fn temp_dir(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("jcctl-sops-{test_name}-{now}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A repository holding one secrets file, plus the identity that can read it.
fn repository(test_name: &str, entries: &[(&str, Node)]) -> (PathBuf, age::x25519::Identity) {
    let dir = temp_dir(test_name);
    let identity = age::x25519::Identity::generate();
    std::fs::write(
        dir.join("secrets.enc.yaml"),
        sops_file(entries, &identity.to_public()),
    )
    .expect("write secrets");
    (dir, identity)
}

const PARKING: &[(&str, &str)] = &[("username", "parking-ingest"), ("password", "s3cr3t-value")];

fn parking_repo(test_name: &str) -> (PathBuf, age::x25519::Identity) {
    repository(
        test_name,
        &[
            ("parking-mqtt-creds", Node::Keys(PARKING)),
            ("portal-session-key", Node::Value("single-value-secret")),
        ],
    )
}

fn reference(name: &str, key: Option<&str>) -> SecretRef {
    SecretRef {
        name: name.to_owned(),
        key: key.map(str::to_owned),
        env_var: None,
    }
}

fn load(dir: &Path, identity: &age::x25519::Identity) -> Result<SecretStore, SopsError> {
    SecretStore::load_dir(dir, std::slice::from_ref(identity))
}

#[test]
fn a_reference_resolves_to_the_value_the_operator_encrypted() {
    let (dir, identity) = parking_repo("resolve");
    let store = load(&dir, &identity).expect("decrypt");

    assert_eq!(
        store
            .resolve(&reference("parking-mqtt-creds", Some("password")))
            .expect("resolve")
            .expose(),
        "s3cr3t-value"
    );
    assert_eq!(
        store
            .resolve(&reference("parking-mqtt-creds", Some("username")))
            .expect("resolve")
            .expose(),
        "parking-ingest"
    );
    assert_eq!(store.len(), 2, "both secrets are indexed");
    assert_eq!(
        store.names().collect::<Vec<_>>(),
        vec!["parking-mqtt-creds", "portal-session-key"]
    );
}

#[test]
fn a_single_value_secret_resolves_without_a_key() {
    let (dir, identity) = parking_repo("single");
    let store = load(&dir, &identity).expect("decrypt");

    assert_eq!(
        store
            .resolve(&reference("portal-session-key", None))
            .expect("resolve")
            .expose(),
        "single-value-secret"
    );
    assert!(matches!(
        store.resolve(&reference("portal-session-key", Some("password"))),
        Err(SopsError::NotKeyed { .. })
    ));
}

#[test]
fn a_reference_that_names_nothing_says_which_reference() {
    let (dir, identity) = parking_repo("missing");
    let store = load(&dir, &identity).expect("decrypt");

    let missing = store.resolve(&reference("no-such-secret", Some("password")));
    assert!(matches!(missing, Err(SopsError::SecretNotFound { .. })));
    assert!(missing.unwrap_err().to_string().contains("no-such-secret"));

    let wrong_key = store.resolve(&reference("parking-mqtt-creds", Some("passphrase")));
    assert!(matches!(wrong_key, Err(SopsError::KeyNotFound { .. })));
    assert!(wrong_key.unwrap_err().to_string().contains("passphrase"));

    assert!(matches!(
        store.resolve(&reference("parking-mqtt-creds", None)),
        Err(SopsError::KeyRequired { .. })
    ));
}

#[test]
fn without_a_key_the_file_is_refused_and_nothing_is_decrypted() {
    let (dir, _identity) = parking_repo("no-key");

    let refused = SecretStore::load_dir(&dir, &[]);
    assert!(matches!(refused, Err(SopsError::NoMatchingIdentity { .. })));
    let message = refused.unwrap_err().to_string();
    assert!(message.contains("secrets.enc.yaml"), "{message}");
    assert!(!message.contains("s3cr3t-value"), "no plaintext in errors");
}

#[test]
fn a_key_that_is_not_a_recipient_is_refused_without_panicking() {
    let (dir, _identity) = parking_repo("wrong-key");
    let stranger = age::x25519::Identity::generate();

    assert!(matches!(
        load(&dir, &stranger),
        Err(SopsError::NoMatchingIdentity { .. })
    ));
}

#[test]
fn an_age_key_file_is_read_and_a_broken_one_is_refused() {
    let dir = temp_dir("key-file");
    let identity = age::x25519::Identity::generate();

    let key_file = dir.join("keys.txt");
    std::fs::write(
        &key_file,
        format!(
            "# created: 2026-09-06T12:00:00Z\n# public key: {}\n{}\n",
            identity.to_public(),
            identity.to_string().expose_secret()
        ),
    )
    .expect("write key file");
    let parsed = identities_from_file(&key_file).expect("parse key file");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].to_public(), identity.to_public());

    let broken = dir.join("broken.txt");
    std::fs::write(&broken, "# only a comment\nnot-an-age-key\n").expect("write broken");
    let Err(refused) = identities_from_file(&broken) else {
        panic!("a key file without a usable key must be refused");
    };
    assert!(matches!(refused, SopsError::InvalidIdentity { .. }));
    assert!(
        !refused.to_string().contains("not-an-age-key"),
        "a key file's contents never appear in an error"
    );

    let comments_only = dir.join("comments.txt");
    std::fs::write(&comments_only, "# public key: none\n\n").expect("write comments");
    assert!(matches!(
        identities_from_file(&comments_only),
        Err(SopsError::InvalidIdentity { .. })
    ));

    assert!(matches!(
        identities_from_file(&dir.join("absent.txt")),
        Err(SopsError::Io { .. })
    ));
}

#[test]
fn a_modified_ciphertext_is_refused() {
    let (dir, identity) = parking_repo("tampered");
    let file = dir.join("secrets.enc.yaml");
    let text = std::fs::read_to_string(&file).expect("read");

    // Flip one base64 character of the password's ciphertext.
    let marker = "password: ENC[AES256_GCM,data:";
    let at = text.find(marker).expect("password literal") + marker.len();
    let mut bytes = text.into_bytes();
    bytes[at] = if bytes[at] == b'A' { b'B' } else { b'A' };
    std::fs::write(&file, bytes).expect("write");

    let refused = load(&dir, &identity);
    assert!(
        matches!(refused, Err(SopsError::Value { .. })),
        "a modified value must not decrypt"
    );
}

#[test]
fn a_value_moved_to_another_field_is_refused() {
    let (dir, identity) = repository(
        "moved",
        &[(
            "parking-mqtt-creds",
            Node::Keys(&[("username", "public-name"), ("password", "s3cr3t-value")]),
        )],
    );
    let file = dir.join("secrets.enc.yaml");
    let text = std::fs::read_to_string(&file).expect("read");

    // Both values are encrypted under the same data key; only the field path they were
    // sealed with keeps the password from being served as the username.
    let password = text
        .lines()
        .find(|l| l.trim_start().starts_with("password:"))
        .expect("password line")
        .trim_start()
        .trim_start_matches("password: ")
        .to_owned();
    let swapped: String = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("username:") {
                format!("    username: {password}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    std::fs::write(&file, swapped).expect("write");

    assert!(matches!(
        load(&dir, &identity),
        Err(SopsError::Value { .. })
    ));
}

#[test]
fn a_value_removed_from_the_file_is_caught_by_the_integrity_check() {
    let (dir, identity) = parking_repo("removed");
    let file = dir.join("secrets.enc.yaml");
    let text = std::fs::read_to_string(&file).expect("read");

    let without_username: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("username:"))
        .map(|line| format!("{line}\n"))
        .collect();
    std::fs::write(&file, without_username).expect("write");

    assert!(
        matches!(load(&dir, &identity), Err(SopsError::MacMismatch { .. })),
        "each value authenticates itself, only the file MAC notices a missing one"
    );
}

#[test]
fn a_plaintext_value_in_a_secrets_file_is_refused() {
    let (dir, identity) = parking_repo("plaintext");
    let file = dir.join("secrets.enc.yaml");
    let text = std::fs::read_to_string(&file).expect("read");
    let leaked: String = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("username:") {
                "    username: parking-ingest\n".to_owned()
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    std::fs::write(&file, leaked).expect("write");

    let refused = load(&dir, &identity);
    assert!(
        matches!(refused, Err(SopsError::NotEncrypted { .. })),
        "CC-06: a secrets file with a plaintext value is refused, not half-read"
    );
    assert!(refused
        .unwrap_err()
        .to_string()
        .contains("parking-mqtt-creds.username"));
}

#[test]
fn a_file_that_is_not_sops_encrypted_is_refused() {
    let dir = temp_dir("not-sops");
    std::fs::write(dir.join("secrets.enc.yaml"), "parking: plaintext\n").expect("write");
    let identity = age::x25519::Identity::generate();

    assert!(matches!(
        load(&dir, &identity),
        Err(SopsError::NotSopsEncrypted { .. })
    ));
}

#[test]
fn two_files_declaring_the_same_secret_are_refused() {
    let dir = temp_dir("duplicate");
    let identity = age::x25519::Identity::generate();
    let recipient = identity.to_public();
    let entries: [(&str, Node); 1] = [("parking-mqtt-creds", Node::Keys(PARKING))];

    std::fs::create_dir_all(dir.join("projects/doprava")).expect("mkdir");
    std::fs::write(
        dir.join("secrets.enc.yaml"),
        sops_file(&entries, &recipient),
    )
    .expect("write");
    std::fs::write(
        dir.join("projects/doprava/secrets.enc.yaml"),
        sops_file(&entries, &recipient),
    )
    .expect("write");

    let refused = load(&dir, &identity);
    assert!(matches!(refused, Err(SopsError::DuplicateSecret { .. })));
    assert!(refused
        .unwrap_err()
        .to_string()
        .contains("parking-mqtt-creds"));
}

#[test]
fn every_encrypted_file_in_the_tree_is_read() {
    let dir = temp_dir("tree");
    let identity = age::x25519::Identity::generate();
    let recipient = identity.to_public();

    std::fs::create_dir_all(dir.join("projects/doprava")).expect("mkdir");
    std::fs::write(
        dir.join("secrets.enc.yaml"),
        sops_file(
            &[("portal-session-key", Node::Value("root-secret"))],
            &recipient,
        ),
    )
    .expect("write");
    std::fs::write(
        dir.join("projects/doprava/mqtt.enc.yaml"),
        sops_file(&[("parking-mqtt-creds", Node::Keys(PARKING))], &recipient),
    )
    .expect("write");

    let store = load(&dir, &identity).expect("decrypt");
    assert_eq!(store.len(), 2);
    assert_eq!(
        store.source_of("parking-mqtt-creds"),
        Some(Path::new("projects/doprava/mqtt.enc.yaml"))
    );
}

#[test]
fn the_manifest_loader_walks_past_encrypted_files() {
    let dir = temp_dir("loader");
    let identity = age::x25519::Identity::generate();
    std::fs::create_dir_all(dir.join("spaces")).expect("mkdir");
    std::fs::write(
        dir.join("secrets.enc.yaml"),
        sops_file(
            &[("portal-session-key", Node::Value("root-secret"))],
            &identity.to_public(),
        ),
    )
    .expect("write");
    std::fs::write(
        dir.join("spaces/doprava.yaml"),
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: doprava\n  namespace: doprava\nspec:\n  isSandbox: false\n",
    )
    .expect("write");

    let repo = Repository::load(&dir).expect("an encrypted file is not a manifest");
    assert_eq!(repo.len(), 1, "only the ContextSpace is a manifest");
}

#[test]
fn no_plaintext_reaches_a_log_line() {
    let (dir, identity) = parking_repo("redaction");
    let store = load(&dir, &identity).expect("decrypt");
    let value = store
        .resolve(&reference("parking-mqtt-creds", Some("password")))
        .expect("resolve");

    assert_eq!(format!("{value:?}"), "SecretValue(redacted)");
    assert!(!format!("{store:?}").contains("s3cr3t-value"));
    assert_eq!(value.len(), "s3cr3t-value".len());
    assert!(!value.is_empty());
}
