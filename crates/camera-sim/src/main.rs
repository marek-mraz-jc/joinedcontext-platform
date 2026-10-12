//! A camera for the demo and the end-to-end check of the camera flow (T-3838, ADR-N-048 §3).
//!
//! `jc-camera-sim keygen <dir>` makes the camera's P-256 key pair: the private half goes to
//! `<dir>/camera.pem` (mode 600) and never leaves it, the public half is printed as the JWK the
//! camera's ServiceAccount declares (PF-47). `jc-camera-sim publish` signs a client assertion with
//! that key, takes an access token from Keycloak for the camera's client `{project}-{account}`,
//! connects to the broker with the token as its password and publishes `JC_CAMERA_COUNT`
//! observations under its own client id. It prints how many the broker acknowledged: the count an
//! end-to-end check compares the written entities with, read from the broker rather than typed.
//!
//! Configuration is the environment, so a container runs it with `-e` and no file:
//! `JC_CAMERA_TOKEN_URL`, `JC_CAMERA_CLIENT_ID`, `JC_CAMERA_KEY` (the PEM), `JC_CAMERA_KID`,
//! `JC_CAMERA_BROKER` (`tls://host:8883` or `tcp://host:1883`), and optionally `JC_CAMERA_CA`
//! (a PEM bundle, the system's by default), `JC_CAMERA_COUNT` (10), `JC_CAMERA_INTERVAL_MS`
//! (1000) and `JC_CAMERA_MEASUREMENT` (`peopleCount`).

mod mqtt;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use p256::SecretKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};
use serde_json::{json, Value};

/// The client-assertion type of RFC 7523, which Keycloak's signed-JWT client authenticator reads.
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
/// How long a signed assertion is good for: one token request.
const ASSERTION_SECONDS: u64 = 60;
/// How long the camera waits for the broker to answer a connection or acknowledge a message.
const SETTLE: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match args.first().map(String::as_str) {
        Some("keygen") => match args.get(1) {
            Some(dir) => keygen(std::path::Path::new(dir)),
            None => Err(Failure::Usage),
        },
        Some("publish") => match Config::from(|name| std::env::var(name).ok()) {
            Ok(config) => run(&config).await,
            Err(e) => Err(e),
        },
        _ => Err(Failure::Usage),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage) => {
            eprintln!("usage: jc-camera-sim keygen <dir> | jc-camera-sim publish (JC_CAMERA_* in the environment)");
            ExitCode::from(64)
        }
        Err(Failure::Refused(why)) => {
            eprintln!("refused: {why}");
            ExitCode::from(2)
        }
        Err(Failure::Error(why)) => {
            eprintln!("error: {why}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq)]
enum Failure {
    Usage,
    /// The token service or the broker said no: the outcome a check of a camera without the
    /// role expects, told apart from a fault.
    Refused(String),
    Error(String),
}

fn error(e: impl std::fmt::Display) -> Failure {
    Failure::Error(e.to_string())
}

fn keygen(dir: &std::path::Path) -> Result<(), Failure> {
    let key = SecretKey::random(&mut OsRng);
    let pem = key.to_pkcs8_pem(LineEnding::LF).map_err(error)?;
    let path = dir.join("camera.pem");
    write_private(&path, pem.as_bytes()).map_err(|e| error(format!("{}: {e}", path.display())))?;
    let kid = format!("camera-{}", hex(&random_bytes::<4>()));
    println!("{}", public_jwk(&key, &kid));
    Ok(())
}

/// Writes the private key readable by its owner alone; an existing file is never overwritten.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(bytes)
}

/// The public half as the ServiceAccount's `jwks` declares it: `kty`, `crv`, `x`, `y`, and no `d`.
fn public_jwk(key: &SecretKey, kid: &str) -> Value {
    let point = key.public_key().to_encoded_point(false);
    let coordinate = |c: Option<&p256::FieldBytes>| c.map(|c| URL_SAFE_NO_PAD.encode(c));
    json!({
        "kty": "EC",
        "crv": "P-256",
        "kid": kid,
        "x": coordinate(point.x()),
        "y": coordinate(point.y()),
        "alg": "ES256",
        "use": "sig",
    })
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug)]
struct Config {
    token_url: String,
    client_id: String,
    key_pem: String,
    kid: String,
    broker: Broker,
    ca: String,
    count: u32,
    interval: Duration,
    measurement: String,
}

#[derive(Debug, PartialEq)]
struct Broker {
    tls: bool,
    host: String,
    port: u16,
}

impl Config {
    fn from(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, Failure> {
        let required = |name: &str| {
            lookup(name)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| Failure::Error(format!("{name} is not set")))
        };
        let number = |name: &str, default: u64| -> Result<u64, Failure> {
            lookup(name).map_or(Ok(default), |raw| {
                raw.trim()
                    .parse::<u64>()
                    .map_err(|_| Failure::Error(format!("{name} is not a whole number: {raw}")))
            })
        };
        let key_path = required("JC_CAMERA_KEY")?;
        let key_pem = std::fs::read_to_string(&key_path)
            .map_err(|e| Failure::Error(format!("JC_CAMERA_KEY {key_path}: {e}")))?;
        let ca_path =
            lookup("JC_CAMERA_CA").unwrap_or_else(|| "/etc/ssl/certs/ca-certificates.crt".into());
        let broker = broker(&required("JC_CAMERA_BROKER")?)?;
        let ca = if broker.tls {
            std::fs::read_to_string(&ca_path)
                .map_err(|e| Failure::Error(format!("JC_CAMERA_CA {ca_path}: {e}")))?
        } else {
            String::new()
        };
        let count = number("JC_CAMERA_COUNT", 10)?;
        if count == 0 || count > 100_000 {
            return Err(Failure::Error(format!(
                "JC_CAMERA_COUNT must be 1 to 100000, not {count}"
            )));
        }
        let measurement = lookup("JC_CAMERA_MEASUREMENT").unwrap_or_else(|| "peopleCount".into());
        if measurement.is_empty() || !measurement.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Failure::Error(format!(
                "JC_CAMERA_MEASUREMENT is one word of letters and digits, not {measurement:?}"
            )));
        }
        Ok(Self {
            token_url: required("JC_CAMERA_TOKEN_URL")?,
            client_id: required("JC_CAMERA_CLIENT_ID")?,
            key_pem,
            kid: required("JC_CAMERA_KID")?,
            broker,
            ca,
            count: count as u32,
            interval: Duration::from_millis(number("JC_CAMERA_INTERVAL_MS", 1000)?),
            measurement,
        })
    }
}

/// `tls://host:8883` or `tcp://host:1883`; nothing else, and always a port.
fn broker(raw: &str) -> Result<Broker, Failure> {
    let refuse = || {
        Failure::Error(format!(
            "JC_CAMERA_BROKER is tls://host:port or tcp://host:port, not {raw}"
        ))
    };
    let (tls, rest) = if let Some(rest) = raw.strip_prefix("tls://") {
        (true, rest)
    } else if let Some(rest) = raw.strip_prefix("tcp://") {
        (false, rest)
    } else {
        return Err(refuse());
    };
    let (host, port) = rest.rsplit_once(':').ok_or_else(refuse)?;
    let port = port
        .trim_end_matches('/')
        .parse::<u16>()
        .map_err(|_| refuse())?;
    if host.is_empty() || host.contains('/') {
        return Err(refuse());
    }
    Ok(Broker {
        tls,
        host: host.to_owned(),
        port,
    })
}

/// An RFC 7523 client assertion: the camera's client is issuer and subject, the token endpoint
/// its audience, signed ES256 with the camera's own key.
fn client_assertion(
    key: &SigningKey,
    kid: &str,
    client_id: &str,
    audience: &str,
    now: u64,
    jti: &str,
) -> String {
    let header = json!({ "alg": "ES256", "typ": "JWT", "kid": kid });
    let claims = json!({
        "iss": client_id,
        "sub": client_id,
        "aud": audience,
        "jti": jti,
        "iat": now,
        "exp": now + ASSERTION_SECONDS,
    });
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature: Signature = key.sign(input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

/// The topic a camera publishes one kind of measurement under: its own client id, which the
/// broker's ACL binds to the token it signed in with.
fn topic(client_id: &str, measurement: &str) -> String {
    format!("cameras/{client_id}/{measurement}")
}

/// One observation, as the camera flow's mapping reads it.
fn observation(frame: u32, observed_at: &str, measurement: &str, value: u64) -> Vec<u8> {
    json!({
        "frame": frame,
        "observedAt": observed_at,
        "measurementType": measurement,
        "value": value,
    })
    .to_string()
    .into_bytes()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `YYYY-MM-DDTHH:MM:SSZ` of a Unix second, without a date library (days from civil, H. Hinnant).
fn rfc3339(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

async fn run(config: &Config) -> Result<(), Failure> {
    let key = SigningKey::from(
        SecretKey::from_pkcs8_pem(&config.key_pem)
            .map_err(|e| error(format!("JC_CAMERA_KEY: {e}")))?,
    );
    let token = token(config, &key).await?;
    let acknowledged = publish(config, &token).await?;
    println!("published {acknowledged} of {}", config.count);
    if acknowledged == config.count {
        Ok(())
    } else {
        Err(error(format!(
            "the broker acknowledged {acknowledged} of {} messages",
            config.count
        )))
    }
}

async fn token(config: &Config, key: &SigningKey) -> Result<String, Failure> {
    let assertion = client_assertion(
        key,
        &config.kid,
        &config.client_id,
        &config.token_url,
        unix_now(),
        &hex(&random_bytes::<16>()),
    );
    let response = reqwest::Client::new()
        .post(&config.token_url)
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", config.client_id.as_str()),
            ("client_assertion_type", ASSERTION_TYPE),
            ("client_assertion", assertion.as_str()),
        ])
        .send()
        .await
        .map_err(|e| error(format!("token request: {e}")))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if status.is_client_error() {
        // Keycloak's own reason (`invalid_client`, `unauthorized_client`), never the assertion.
        return Err(Failure::Refused(format!(
            "the token service answered {status}: {}",
            body["error"].as_str().unwrap_or("no reason given")
        )));
    }
    body["access_token"]
        .as_str()
        .filter(|_| status.is_success())
        .map(str::to_owned)
        .ok_or_else(|| {
            error(format!(
                "the token service answered {status} without an access token"
            ))
        })
}

async fn publish(config: &Config, token: &str) -> Result<u32, Failure> {
    let address = (config.broker.host.as_str(), config.broker.port);
    let tcp = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|e| error(format!("broker connection: {e}")))?;
    if !config.broker.tls {
        return session(tcp, config, token).await;
    }
    let mut roots = rustls::RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(config.ca.as_bytes()) {
        let certificate = certificate.map_err(|e| error(format!("JC_CAMERA_CA: {e}")))?;
        roots
            .add(certificate)
            .map_err(|e| error(format!("JC_CAMERA_CA: {e}")))?;
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| error(format!("TLS: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = ServerName::try_from(config.broker.host.clone())
        .map_err(|e| error(format!("JC_CAMERA_BROKER host: {e}")))?;
    let stream = tokio_rustls::TlsConnector::from(Arc::new(tls))
        .connect(name, tcp)
        .await
        .map_err(|e| error(format!("broker connection: TLS: {e}")))?;
    session(stream, config, token).await
}

/// Connects, then publishes one message at a time and waits for its PUBACK, so the count is
/// what the broker took. A broker that drops the connection (a publish its ACL refused) ends
/// the run with the count so far.
async fn session(
    mut stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    config: &Config,
    token: &str,
) -> Result<u32, Failure> {
    let io = |e: std::io::Error| error(format!("broker connection: {e}"));
    let connect = mqtt::connect(&config.client_id, &config.client_id, token, 30);
    mqtt::send(&mut stream, &connect).await.map_err(io)?;
    match tokio::time::timeout(SETTLE, mqtt::read(&mut stream)).await {
        Err(_) => return Err(error("the broker did not answer the connection")),
        Ok(Err(e)) => return Err(io(e)),
        Ok(Ok(None)) => return Err(Failure::Refused("the broker closed the connection".into())),
        Ok(Ok(Some(packet))) if packet.kind != mqtt::CONNACK => {
            return Err(error(format!(
                "the broker answered packet type {}",
                packet.kind
            )))
        }
        Ok(Ok(Some(packet))) => {
            if let Some(reason) = mqtt::refusal(&packet.body) {
                return Err(Failure::Refused(format!(
                    "the broker refused the connection: {reason}"
                )));
            }
        }
    }

    let topic = topic(&config.client_id, &config.measurement);
    let mut acknowledged = 0u32;
    for frame in 1..=config.count {
        if frame > 1 {
            tokio::time::sleep(config.interval).await;
        }
        let value = u64::from(random_bytes::<1>()[0] % 20);
        let body = observation(frame, &rfc3339(unix_now()), &config.measurement, value);
        // Packet ids are 1..=65535; a run longer than that reuses them, one in flight at a time.
        let id = (frame - 1) % 65_535 + 1;
        let id = id as u16;
        if mqtt::send(&mut stream, &mqtt::publish(&topic, id, &body))
            .await
            .is_err()
        {
            break;
        }
        let acked = loop {
            match tokio::time::timeout(SETTLE, mqtt::read(&mut stream)).await {
                Ok(Ok(Some(packet))) if packet.kind == mqtt::PUBACK => {
                    break mqtt::acknowledged(&packet.body) == Some(id)
                }
                Ok(Ok(Some(_))) => continue,
                _ => break false,
            }
        };
        if !acked {
            break;
        }
        acknowledged += 1;
    }
    Ok(acknowledged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;
    use std::collections::BTreeMap;

    fn decode(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).expect("base64url")).expect("json")
    }

    #[test]
    fn the_assertion_is_signed_by_the_camera_key_for_its_own_client_and_the_token_endpoint() {
        let key = SigningKey::from(SecretKey::random(&mut OsRng));
        let jwt = client_assertion(
            &key,
            "cam-1",
            "espoo-cam-north",
            "https://id/token",
            1_000,
            "j1",
        );
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            decode(parts[0]),
            json!({ "alg": "ES256", "typ": "JWT", "kid": "cam-1" })
        );
        let claims = decode(parts[1]);
        assert_eq!(claims["iss"], "espoo-cam-north");
        assert_eq!(claims["sub"], "espoo-cam-north");
        assert_eq!(claims["aud"], "https://id/token");
        assert_eq!(claims["exp"], 1_060);
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).expect("b64"))
            .expect("64 bytes");
        let input = format!("{}.{}", parts[0], parts[1]);
        VerifyingKey::from(&key)
            .verify(input.as_bytes(), &signature)
            .expect("the camera's key signed it");
        let other = SigningKey::from(SecretKey::random(&mut OsRng));
        assert!(VerifyingKey::from(&other)
            .verify(input.as_bytes(), &signature)
            .is_err());
    }

    #[test]
    fn the_public_jwk_has_the_point_and_never_the_private_scalar() {
        let key = SecretKey::random(&mut OsRng);
        let jwk = public_jwk(&key, "cam-1");
        assert_eq!(jwk["kty"], "EC");
        assert_eq!(jwk["crv"], "P-256");
        assert_eq!(jwk["x"].as_str().map(str::len), Some(43));
        assert_eq!(jwk["y"].as_str().map(str::len), Some(43));
        assert!(jwk.get("d").is_none());
    }

    #[test]
    fn the_private_key_file_is_the_owners_alone_and_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("camera-sim-{}", hex(&random_bytes::<6>())));
        std::fs::create_dir(&dir).expect("a temp dir");
        keygen(&dir).expect("keygen");
        let path = dir.join("camera.pem");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("written")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(SecretKey::from_pkcs8_pem(&std::fs::read_to_string(&path).expect("pem")).is_ok());
        assert!(
            keygen(&dir).is_err(),
            "a second keygen would replace the camera's identity"
        );
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn a_message_is_what_the_camera_flow_maps_and_goes_under_the_cameras_own_id() {
        assert_eq!(
            topic("espoo-cam-north", "peopleCount"),
            "cameras/espoo-cam-north/peopleCount"
        );
        let body: Value =
            serde_json::from_slice(&observation(7, "2026-10-12T09:00:00Z", "peopleCount", 3))
                .expect("json");
        assert_eq!(
            body,
            json!({ "frame": 7, "observedAt": "2026-10-12T09:00:00Z", "measurementType": "peopleCount", "value": 3 })
        );
    }

    #[test]
    fn a_broker_address_needs_a_scheme_a_host_and_a_port() {
        assert_eq!(
            broker("tls://cams.espoo.mqtt.dev.example:8883"),
            Ok(Broker {
                tls: true,
                host: "cams.espoo.mqtt.dev.example".into(),
                port: 8883
            })
        );
        assert_eq!(
            broker("tcp://localhost:1883"),
            Ok(Broker {
                tls: false,
                host: "localhost".into(),
                port: 1883
            })
        );
        for bad in [
            "mqtt://h:1883",
            "tls://h",
            "tls://:8883",
            "tls://h:99999",
            "h:8883",
        ] {
            assert!(broker(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_configuration_names_what_is_missing_or_out_of_range() {
        let dir = std::env::temp_dir().join(format!("camera-sim-{}", hex(&random_bytes::<6>())));
        std::fs::create_dir(&dir).expect("a temp dir");
        keygen(&dir).expect("keygen");
        let key = dir.join("camera.pem").display().to_string();
        let base: BTreeMap<&str, String> = [
            ("JC_CAMERA_TOKEN_URL", "https://id/token".to_owned()),
            ("JC_CAMERA_CLIENT_ID", "espoo-cam-north".to_owned()),
            ("JC_CAMERA_KEY", key),
            ("JC_CAMERA_KID", "cam-1".to_owned()),
            ("JC_CAMERA_BROKER", "tcp://localhost:1883".to_owned()),
        ]
        .into();
        let with = |change: &dyn Fn(&mut BTreeMap<&str, String>)| {
            let mut env = base.clone();
            change(&mut env);
            Config::from(move |name| env.get(name).cloned())
        };
        let config = with(&|_| {}).expect("a whole configuration");
        assert_eq!(
            (config.count, config.interval, config.measurement.as_str()),
            (10, Duration::from_secs(1), "peopleCount")
        );
        assert_eq!(
            with(&|env| {
                env.remove("JC_CAMERA_KID");
            })
            .err(),
            Some(Failure::Error("JC_CAMERA_KID is not set".into()))
        );
        assert!(with(&|env| {
            env.insert("JC_CAMERA_COUNT", "0".into());
        })
        .is_err());
        assert!(
            with(&|env| {
                env.insert("JC_CAMERA_MEASUREMENT", "a/#".into());
            })
            .is_err(),
            "a measurement may not reach another topic"
        );
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn a_unix_second_reads_as_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_791_795_600), "2026-10-12T09:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }
}
