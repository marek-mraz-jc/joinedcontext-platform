//! The outbound HTTP guard: one place that decides which addresses a call to a URL that came from
//! a manifest, a catalogue entry or a user may reach (T-4776, AG-65, T-1304).
//!
//! A host name is all a manifest can name, and what it resolves to is decided by whoever runs its
//! DNS. A name answering `127.0.0.1`, `10.x` or `169.254.169.254` would hand the caller the
//! cluster's own services and the cloud's metadata. The addresses are therefore judged where they
//! are used: [`PublicOnly`] is the resolver `reqwest` connects through, so no second DNS answer (a
//! rebinding) slips between the check and the connection. An address written into the URL is
//! never resolved, so [`check`] judges it before the first request and [`redirects`] on every hop.
//! [`client`] puts the pieces together with timeouts and without the environment's proxy, which
//! would otherwise resolve the name on the guard's behalf; [`read_capped`] bounds the body.
//!
//! A URL from the installation's own configuration (the Portal, the realm, the broker) is no
//! input and keeps a plain client with a timeout: those hosts are private by design.

use std::error::Error;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::Url;

pub use crate::address::is_public;

/// How many redirects a guarded client follows, each judged like the first URL.
pub const MAX_REDIRECTS: usize = 5;

/// A name whose every address is one an outbound call must not reach.
#[derive(Debug)]
pub struct PrivateAddress {
    /// The name that resolved to no public address.
    pub host: String,
}

impl fmt::Display for PrivateAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "host '{}' resolves only to private, loopback or link-local addresses; outbound \
             calls reach public hosts only (AG-65)",
            self.host
        )
    }
}

impl Error for PrivateAddress {}

/// Why a URL is not called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not `http` or `https`, or no host.
    Scheme(String),
    /// An address written into the URL that is not on the public internet.
    Private(IpAddr),
    /// A redirect from `https` to plain `http`.
    Downgrade,
    /// More than [`MAX_REDIRECTS`] hops.
    TooManyRedirects,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheme(url) => write!(f, "'{url}' is no http(s) URL with a host"),
            Self::Private(ip) => write!(
                f,
                "'{ip}' is a private, loopback or link-local address; outbound calls reach \
                 public hosts only (AG-65)"
            ),
            Self::Downgrade => f.write_str("a redirect from https to plain http is not followed"),
            Self::TooManyRedirects => {
                write!(
                    f,
                    "more than {MAX_REDIRECTS} redirects; the chain is not followed"
                )
            }
        }
    }
}

impl Error for Refusal {}

/// The system resolver with every non-public address removed; a name left with none fails.
#[derive(Debug, Default, Clone, Copy)]
pub struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|addr| is_public(addr.ip())).collect();
            if public.is_empty() {
                return Err(Box::new(PrivateAddress { host }) as Box<dyn Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// The resolver's refusal, when it is what failed a request: a caller answers it as a refusal
/// rather than as an upstream that could not be reached.
pub fn refused<'a>(err: &'a (dyn Error + 'static)) -> Option<&'a PrivateAddress> {
    let mut cause: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(current) = cause {
        if let Some(found) = current.downcast_ref::<PrivateAddress>() {
            return Some(found);
        }
        cause = current.source();
    }
    None
}

/// Judges a URL before it is called: `http` or `https` with a host, and an address written into
/// it on the public internet. A name is judged by [`PublicOnly`] when it is resolved.
pub fn check(url: &Url) -> Result<(), Refusal> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Refusal::Scheme(url.to_string()));
    }
    let Some(host) = url.host_str() else {
        return Err(Refusal::Scheme(url.to_string()));
    };
    // The URL keeps an IPv6 address in brackets; a name parses as no address at all.
    match host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        Ok(literal) if !is_public(literal) => Err(Refusal::Private(literal)),
        _ => Ok(()),
    }
}

/// The redirect rule of a guarded client: at most `max` hops, never from `https` to `http`, and
/// each hop's URL judged by [`check`]; a named hop is resolved through [`PublicOnly`] again.
pub fn redirects(max: usize) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > max {
            return attempt.error(Refusal::TooManyRedirects);
        }
        let downgrade = attempt.url().scheme() == "http"
            && attempt.previous().iter().any(|hop| hop.scheme() == "https");
        if downgrade {
            return attempt.error(Refusal::Downgrade);
        }
        match check(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(refusal) => attempt.error(refusal),
        }
    })
}

/// A client builder for URLs that came from outside the installation: public addresses only after
/// resolution, no proxy from the environment, [`redirects`] with [`MAX_REDIRECTS`], and a connect
/// and a total timeout. A caller still runs [`check`] on the first URL and reads the body with
/// [`read_capped`].
pub fn client(connect: Duration, total: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(PublicOnly))
        .redirect(redirects(MAX_REDIRECTS))
        .connect_timeout(connect)
        .timeout(total)
}

/// Why a body was not read whole.
#[derive(Debug)]
pub enum BodyError {
    /// The body is longer than the cap.
    TooLarge(usize),
    /// The transport failed while reading.
    Transport(reqwest::Error),
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(max) => write!(f, "the answer is longer than {max} bytes"),
            Self::Transport(err) => write!(f, "the answer could not be read: {}", err),
        }
    }
}

impl Error for BodyError {}

/// The body of `response`, refused as soon as it passes `max` bytes: a declared length over the
/// cap is refused before a byte is read, and an undeclared one stops at the first chunk past it.
pub async fn read_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(BodyError::TooLarge(max));
    }
    let mut body = Vec::new();
    // A transport error names the URL it was reading; an internal service address never reaches
    // the text a caller relays.
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| BodyError::Transport(e.without_url()))?
    {
        if body.len() + chunk.len() > max {
            return Err(BodyError::TooLarge(max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn a_url_naming_an_inside_address_or_another_scheme_is_refused() {
        for (url, refusal) in [
            ("http://169.254.169.254/latest/meta-data", "169.254.169.254"),
            ("http://[::1]:8080/", "::1"),
            ("https://[fd12::1]/", "fd12::1"),
            ("http://10.0.0.1/", "10.0.0.1"),
        ] {
            let parsed = Url::parse(url).expect("a URL");
            assert_eq!(
                check(&parsed),
                Err(Refusal::Private(refusal.parse().expect("an address"))),
                "{url}"
            );
        }
        let file = Url::parse("file:///etc/passwd").expect("a URL");
        assert!(matches!(check(&file), Err(Refusal::Scheme(_))));
        for url in ["https://opendata.dev.joinedcontext.com/", "http://1.1.1.1/"] {
            assert_eq!(check(&Url::parse(url).expect("a URL")), Ok(()), "{url}");
        }
    }

    /// `localhost` is the rebinding in miniature, a name whose DNS answer is loopback.
    #[tokio::test]
    async fn a_name_that_resolves_to_loopback_is_refused_by_the_resolver() {
        let err = match PublicOnly
            .resolve("localhost".parse().expect("a name"))
            .await
        {
            Ok(addrs) => panic!("resolved to {:?}", addrs.collect::<Vec<_>>()),
            Err(err) => err,
        };
        let found = refused(err.as_ref()).expect("the resolver's own refusal");
        assert_eq!(found.host, "localhost");
    }

    /// One local server answering every connection with `head`, then `body` forever when
    /// `endless`. Reached by its literal loopback address, which only the test's direct
    /// `reqwest` call may do: [`check`] refuses it for every real caller.
    async fn serve(head: String, endless: bool) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let addr = listener.local_addr().expect("an address");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let head = head.clone();
                tokio::spawn(async move {
                    let mut request = [0u8; 2048];
                    let _ = socket.read(&mut request).await;
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let chunk = format!("{:x}\r\n{}\r\n", 1024, "x".repeat(1024));
                    while endless && socket.write_all(chunk.as_bytes()).await.is_ok() {}
                });
            }
        });
        Url::parse(&format!("http://{addr}/")).expect("a URL")
    }

    fn guarded() -> reqwest::Client {
        client(Duration::from_secs(2), Duration::from_secs(5))
            .build()
            .expect("the guarded client builds")
    }

    async fn redirect_to(location: &str) -> reqwest::Error {
        let head =
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n");
        let url = serve(head, false).await;
        match guarded().get(url).send().await {
            Ok(response) => panic!("followed to {}", response.url()),
            Err(err) => err,
        }
    }

    fn refusal_of(err: &reqwest::Error) -> Option<&Refusal> {
        let mut cause: Option<&(dyn Error + 'static)> = Some(err);
        while let Some(current) = cause {
            if let Some(found) = current.downcast_ref::<Refusal>() {
                return Some(found);
            }
            cause = current.source();
        }
        None
    }

    #[tokio::test]
    async fn a_redirect_to_loopback_is_not_followed() {
        let err = redirect_to("http://127.0.0.1:1/admin").await;
        assert!(err.is_redirect(), "{err:?}");
        assert_eq!(
            refusal_of(&err),
            Some(&Refusal::Private("127.0.0.1".parse().expect("an address")))
        );
    }

    #[tokio::test]
    async fn a_redirect_to_the_metadata_address_is_not_followed() {
        let err = redirect_to("http://169.254.169.254/latest/meta-data/").await;
        assert_eq!(
            refusal_of(&err),
            Some(&Refusal::Private(
                "169.254.169.254".parse().expect("an address")
            ))
        );
    }

    #[tokio::test]
    async fn a_redirect_to_a_name_resolving_to_loopback_is_not_followed() {
        let err = redirect_to("http://localhost:1/").await;
        let found = refused(&err).expect("the resolver's refusal");
        assert_eq!(found.host, "localhost");
    }

    #[tokio::test]
    async fn an_endless_body_stops_at_the_cap() {
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_owned();
        let url = serve(head, true).await;
        let response = guarded().get(url).send().await.expect("an answer");
        let read = tokio::time::timeout(Duration::from_secs(3), read_capped(response, 64 * 1024))
            .await
            .expect("the cap ends the read before the timeout");
        assert!(matches!(read, Err(BodyError::TooLarge(65536))), "{read:?}");
    }

    #[tokio::test]
    async fn a_declared_length_over_the_cap_is_refused_unread() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n".to_owned();
        let url = serve(head, false).await;
        let response = guarded().get(url).send().await.expect("an answer");
        assert!(matches!(
            read_capped(response, 1024).await,
            Err(BodyError::TooLarge(1024))
        ));
    }

    #[tokio::test]
    async fn a_body_under_the_cap_is_read_whole() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_owned();
        let url = serve(head, false).await;
        let response = guarded().get(url).send().await.expect("an answer");
        assert_eq!(
            read_capped(response, 1024).await.expect("the body"),
            b"hello"
        );
    }

    #[tokio::test]
    async fn a_broken_body_names_no_url() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhello".to_owned();
        let url = serve(head, false).await;
        let at = url.join("internal-service").expect("a URL");
        let response = guarded().get(at.clone()).send().await.expect("an answer");
        let err = read_capped(response, 1024)
            .await
            .expect_err("the body breaks off");
        assert!(matches!(err, BodyError::Transport(_)), "{err:?}");
        let text = err.to_string();
        assert!(!text.contains(at.as_str()), "{text}");
        assert!(!text.contains("internal-service"), "{text}");
    }
}
