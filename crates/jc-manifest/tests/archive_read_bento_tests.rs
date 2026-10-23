//! T-4752: the rendered archive read (PL-72) run in the Bento the runner pins, over an archive
//! built here: every member that keeps its directory's word arrives whole, and one that
//! inflates past its declared size or fails its CRC-32 is refused. Only the member request is
//! replaced, by a slice of the same bytes at the same `Range`.

use std::io::Write;
use std::process::Command;

use base64::Engine;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::DataSourceSpec;
use jc_manifest::bento::{render, InputContext};
use serde_json::{json, Value};

/// `components/pipeline-runner/images.yaml` of joinedcontext-deployment.
const BENTO: &str = "ghcr.io/warpstreamlabs/bento:1.22.0@sha256:ef674bebae45793c982c350d8f37c2c7661133c1dc65a5517fb977b36dc09841";

struct Member {
    name: &'static str,
    text: Vec<u8>,
    deflate: bool,
    /// The data-descriptor flag, with zero sizes and CRC in the local header.
    descriptor: bool,
    /// The size the directory declares, when it lies.
    declared: Option<u32>,
    crc_off: bool,
}

fn member(name: &'static str, text: &[u8]) -> Member {
    Member {
        name,
        text: text.to_vec(),
        deflate: true,
        descriptor: false,
        declared: None,
        crc_off: false,
    }
}

fn deflated(text: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text).expect("deflates");
    encoder.finish().expect("deflates")
}

/// A zip archive as a server holds it, written field by field.
fn archive(members: &[Member]) -> Vec<u8> {
    let (mut out, mut directory) = (Vec::new(), Vec::new());
    for m in members {
        let data = if m.deflate {
            deflated(&m.text)
        } else {
            m.text.clone()
        };
        let crc = crc32fast::hash(&m.text) ^ u32::from(m.crc_off);
        let size = m
            .declared
            .unwrap_or(u32::try_from(m.text.len()).expect("small"));
        let csize = u32::try_from(data.len()).expect("small");
        let flags: u16 = if m.descriptor { 8 } else { 0 };
        let method: u16 = if m.deflate { 8 } else { 0 };
        let offset = u32::try_from(out.len()).expect("small");
        let name_len = u16::try_from(m.name.len()).expect("short");
        let (local_crc, local_csize, local_size) = if m.descriptor {
            (0, 0, 0)
        } else {
            (crc, csize, size)
        };
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes());
        out.extend(flags.to_le_bytes());
        out.extend(method.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend(local_crc.to_le_bytes());
        out.extend(local_csize.to_le_bytes());
        out.extend(local_size.to_le_bytes());
        out.extend(name_len.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(m.name.as_bytes());
        out.extend(&data);
        if m.descriptor {
            out.extend(0x0807_4b50u32.to_le_bytes());
            out.extend(crc.to_le_bytes());
            out.extend(csize.to_le_bytes());
            out.extend(size.to_le_bytes());
        }
        directory.extend(0x0201_4b50u32.to_le_bytes());
        directory.extend(20u16.to_le_bytes());
        directory.extend(20u16.to_le_bytes());
        directory.extend(flags.to_le_bytes());
        directory.extend(method.to_le_bytes());
        directory.extend([0u8; 4]);
        directory.extend(crc.to_le_bytes());
        directory.extend(csize.to_le_bytes());
        directory.extend(size.to_le_bytes());
        directory.extend(name_len.to_le_bytes());
        directory.extend([0u8; 12]);
        directory.extend(offset.to_le_bytes());
        directory.extend(m.name.as_bytes());
    }
    let count = u16::try_from(members.len()).expect("few");
    let (cd_size, cd_at) = (
        u32::try_from(directory.len()).expect("small"),
        u32::try_from(out.len()).expect("small"),
    );
    out.extend(directory);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(count.to_le_bytes());
    out.extend(count.to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_at.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

/// What the read hands the author's steps for `members` of `zip`, by member name: its size, or
/// the refusal the catch logged.
fn read(zip: &[u8], members: &[&str]) -> Vec<Value> {
    let source = ResourceEnvelope::<DataSourceSpec>::from_yaml(&format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: DataSource\nmetadata: {{ name: gtfs, namespace: helsinki }}\n\
         spec:\n  type: http\n  http:\n    url: https://example.org/hsl.zip\n    archive:\n      format: zip\n      \
         members: {}\n      maxBytes: 1048576\n",
        serde_json::to_string(members).expect("names")
    ))
    .expect("parses");
    source.validate().expect("valid");
    let context = InputContext {
        source: "gtfs",
        project: "helsinki",
        pipeline: "gtfs-routes",
        pipeline_spec: None,
    };
    let rendered: Value =
        serde_norway::from_str(&render("", &source.spec, &context).expect("renders"))
            .expect("yaml");
    let mut processors = rendered["pipeline"]["processors"].clone();
    let encoded = base64::engine::general_purpose::STANDARD.encode(zip);
    // The member request, as a server that honours Range answers it.
    processors[4]["for_each"][0]["try"][1] = json!({"mapping": format!(
        "let at = @range.re_find_all_submatch(\"^bytes=([0-9]+)-([0-9]+)$\").index(0)\n\
         let whole = \"{encoded}\".decode(\"base64\")\n\
         let end = [$at.index(2).number() + 1, $whole.length()].min()\n\
         root = $whole.slice($at.index(1).number(), $end)"
    )});
    // A refusal is kept, named, instead of dropped.
    let kept = json!({"mapping": "meta refused = error()\nroot = \"\""});
    processors[3]["catch"][1] = kept.clone();
    processors[4]["for_each"][1]["catch"][1] = kept;
    processors.as_array_mut().expect("steps").push(json!({"mapping":
        "root = if @refused.or(\"\") != \"\" { {\"name\": @archive_filename.or(\"\"), \"refused\": @refused} } else { {\"name\": @archive_filename, \"bytes\": content().length()} }"}));
    let config = json!({
        "input": {"generate": {"count": 1, "interval": "", "mapping": format!("root = \"{encoded}\".decode(\"base64\")")}},
        "pipeline": {"processors": processors},
        "output": {"stdout": {}},
        "logger": {"level": "none"}
    });
    let run = Command::new("docker")
        .args(["run", "--rm", "-e"])
        .arg(format!("CONFIG={config}"))
        .args([
            "--entrypoint",
            "sh",
            BENTO,
            "-c",
            r#"printf "%s" "$CONFIG" > /tmp/c.json && exec /bento -c /tmp/c.json"#,
        ])
        .output()
        .expect("this test runs the runner's Bento in Docker; install Docker to run it");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let mut rows: Vec<Value> = String::from_utf8(run.stdout)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("not a row: {line:.300}")))
        .collect();
    rows.sort_by_key(|row| row["name"].as_str().unwrap_or_default().to_owned());
    rows
}

#[test]
fn every_member_that_keeps_its_directorys_word_arrives_whole() {
    let rows_text = "stop_id,stop_name\n".to_owned() + &"1040601,Kamppi\n".repeat(4000);
    let mut stored = member("agency.txt", b"agency_id,agency_name\nHSL,HSL\n");
    stored.deflate = false;
    let mut streamed = member("routes.txt", rows_text.as_bytes());
    streamed.descriptor = true;
    let zip = archive(&[
        stored,
        member("stops.txt", rows_text.as_bytes()),
        streamed,
        member("trips.txt", b"never asked for"),
    ]);
    let rows = read(&zip, &["agency.txt", "stops.txt", "routes.txt"]);
    let size = rows_text.len();
    assert_eq!(
        rows,
        [
            json!({"name": "agency.txt", "bytes": 30}),
            json!({"name": "routes.txt", "bytes": size}),
            json!({"name": "stops.txt", "bytes": size}),
        ]
    );
}

#[test]
fn a_member_that_inflates_past_or_short_of_its_declared_size_or_fails_its_crc_is_refused() {
    // Five megabytes of zeros deflate to a few kilobytes; the directory says 100 bytes.
    let mut bomb = member("shapes.txt", &vec![b'0'; 5_000_000]);
    bomb.declared = Some(100);
    let mut corrupt = member("stops.txt", b"stop_id\n1\n2\n");
    corrupt.crc_off = true;
    let mut short = member("routes.txt", b"route_id\n1004\n");
    short.declared = Some(64);
    let zip = archive(&[
        bomb,
        corrupt,
        short,
        member("agency.txt", b"agency_id\nHSL\n"),
    ]);
    let rows = read(
        &zip,
        &["shapes.txt", "stops.txt", "routes.txt", "agency.txt"],
    );
    // The member that keeps its word still arrives whole beside the refused ones.
    assert_eq!(rows[0], json!({"name": "agency.txt", "bytes": 14}));
    let refused: Vec<(&str, &str)> = rows[1..]
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap_or_default(),
                row["refused"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        refused,
        [
            ("routes.txt", "unexpected EOF"),
            ("shapes.txt", "zip: not a valid zip file"),
            ("stops.txt", "zip: checksum error"),
        ],
        "{rows:?}"
    );
}
