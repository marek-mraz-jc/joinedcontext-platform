//! The part of MQTT 3.1.1 a camera needs: CONNECT with a user name and password, PUBLISH at
//! QoS 1 and its PUBACK. rumqttc would bring a rustls-webpki with open advisories (cargo-deny),
//! and three packets fit on a page.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// CONNECT with a clean session, the user name and password flags set, and no will.
pub fn connect(client_id: &str, username: &str, password: &str, keep_alive: u16) -> Vec<u8> {
    let mut body = Vec::new();
    string(&mut body, "MQTT");
    body.push(4); // protocol level 3.1.1
    body.push(0b1100_0010); // user name, password, clean session
    body.extend_from_slice(&keep_alive.to_be_bytes());
    string(&mut body, client_id);
    string(&mut body, username);
    string(&mut body, password);
    packet(0x10, &body)
}

/// PUBLISH at QoS 1, not retained, first delivery.
pub fn publish(topic: &str, packet_id: u16, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    string(&mut body, topic);
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(payload);
    packet(0x32, &body)
}

/// A packet the broker sent: its type (the high nibble) and its variable header and payload.
#[derive(Debug, PartialEq)]
pub struct Incoming {
    pub kind: u8,
    pub body: Vec<u8>,
}

/// Reads one packet; `None` when the broker closed the connection between packets.
pub async fn read(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<Incoming>> {
    let mut first = [0u8; 1];
    if stream.read(&mut first).await? == 0 {
        return Ok(None);
    }
    let mut length = 0usize;
    for shift in 0..4 {
        let byte = stream.read_u8().await?;
        length |= usize::from(byte & 0x7f) << (7 * shift);
        if byte & 0x80 == 0 {
            let mut body = vec![0u8; length];
            stream.read_exact(&mut body).await?;
            return Ok(Some(Incoming {
                kind: first[0] >> 4,
                body,
            }));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "a remaining length longer than four bytes",
    ))
}

pub async fn send(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> std::io::Result<()> {
    stream.write_all(bytes).await?;
    stream.flush().await
}

pub const CONNACK: u8 = 2;
pub const PUBACK: u8 = 4;

/// What a CONNACK's return code means, `None` when the connection was accepted.
pub fn refusal(connack: &[u8]) -> Option<&'static str> {
    match connack.get(1) {
        Some(0) => None,
        Some(1) => Some("unacceptable protocol version"),
        Some(2) => Some("client identifier rejected"),
        Some(3) => Some("server unavailable"),
        Some(4) => Some("bad user name or password"),
        Some(5) => Some("not authorized"),
        _ => Some("malformed CONNACK"),
    }
}

/// The packet id a PUBACK acknowledges.
pub fn acknowledged(puback: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*puback.first()?, *puback.get(1)?]))
}

fn string(out: &mut Vec<u8>, value: &str) {
    // Every string here is a client id, a topic or a token, far below 64 KiB.
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn packet(header: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![header];
    let mut length = body.len();
    loop {
        let byte = (length % 128) as u8;
        length /= 128;
        out.push(if length > 0 { byte | 0x80 } else { byte });
        if length == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_carries_the_client_the_user_and_the_token_with_a_clean_session() {
        let bytes = connect("espoo-cam", "espoo-cam", "t0k", 30);
        assert_eq!(bytes[0], 0x10);
        assert_eq!(usize::from(bytes[1]), bytes.len() - 2);
        assert_eq!(&bytes[2..8], b"\x00\x04MQTT");
        assert_eq!(&bytes[8..12], &[4, 0b1100_0010, 0, 30]);
        assert_eq!(
            &bytes[12..],
            b"\x00\x09espoo-cam\x00\x09espoo-cam\x00\x03t0k"
        );
    }

    #[test]
    fn a_long_publish_encodes_its_length_in_several_bytes() {
        let payload = vec![b'x'; 300];
        let bytes = publish("a/b", 7, &payload);
        // 2 + 3 (topic) + 2 (id) + 300 = 307 = 0b10_0110011 → 0xb3 0x02
        assert_eq!(&bytes[..3], &[0x32, 0xb3, 0x02]);
        assert_eq!(&bytes[3..10], b"\x00\x03a/b\x00\x07");
        assert_eq!(bytes.len(), 3 + 307);
    }

    #[tokio::test]
    async fn reads_a_packet_a_closed_stream_and_refuses_a_runaway_length() {
        let mut stream: &[u8] = &[0x40, 0x02, 0x00, 0x07, 0x20, 0x02, 0x00, 0x05];
        let puback = read(&mut stream).await.unwrap().unwrap();
        assert_eq!((puback.kind, acknowledged(&puback.body)), (PUBACK, Some(7)));
        let connack = read(&mut stream).await.unwrap().unwrap();
        assert_eq!(connack.kind, CONNACK);
        assert_eq!(refusal(&connack.body), Some("not authorized"));
        assert_eq!(read(&mut stream).await.unwrap(), None);

        let mut runaway: &[u8] = &[0x40, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert!(read(&mut runaway).await.is_err());
        let mut truncated: &[u8] = &[0x40, 0x02, 0x00];
        assert!(read(&mut truncated).await.is_err());
        assert_eq!(refusal(&[0, 0]), None);
        assert_eq!(refusal(&[]), Some("malformed CONNACK"));
    }
}
