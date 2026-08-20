//! Magnet URI parsing. Extracts the infohash, which TorBox needs for `checkcached`.

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct Magnet {
    /// Lowercase hex infohash, always 40 chars.
    pub hash: String,
    pub display_name: Option<String>,
}

/// Percent-decoding, enough for magnet `dn=` values.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn looks_like_magnet(s: &str) -> bool {
    s.trim().to_ascii_lowercase().starts_with("magnet:?")
}

/// Parses a magnet URI. Handles both infohash encodings: 40-char hex and
/// 32-char base32 (common from older indexers), normalizing to lowercase hex.
pub fn parse(input: &str) -> Result<Magnet> {
    let uri = input.trim();
    if !looks_like_magnet(uri) {
        bail!("not a magnet link (must start with `magnet:?`)");
    }
    let query = &uri[uri.find('?').context("malformed magnet")? + 1..];

    let mut hash = None;
    let mut display_name = None;

    for pair in query.split('&') {
        let (key, value) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        match key {
            "xt" if hash.is_none() => {
                let value = percent_decode(value);
                if let Some(raw) = value.strip_prefix("urn:btih:") {
                    hash = Some(normalize_infohash(raw)?);
                }
                // urn:btmh: is BitTorrent v2; TorBox keys on v1 infohashes.
            }
            "dn" => display_name = Some(percent_decode(value)),
            _ => {}
        }
    }

    let hash = hash.context("magnet has no `xt=urn:btih:` infohash (v2-only magnets are unsupported)")?;
    Ok(Magnet { hash, display_name })
}

fn normalize_infohash(raw: &str) -> Result<String> {
    let raw = raw.trim();
    match raw.len() {
        40 => {
            hex::decode(raw).context("infohash is not valid hex")?;
            Ok(raw.to_ascii_lowercase())
        }
        32 => {
            let bytes = data_encoding::BASE32_NOPAD
                .decode(raw.to_ascii_uppercase().as_bytes())
                .context("infohash is not valid base32")?;
            Ok(hex::encode(bytes))
        }
        n => bail!("infohash has unexpected length {n} (want 40 hex or 32 base32)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex() {
        let m = parse("magnet:?xt=urn:btih:C12FE1C06BBA254A9DC9F519B335AA7C1367A88A&dn=Some+Movie").unwrap();
        assert_eq!(m.hash, "c12fe1c06bba254a9dc9f519b335aa7c1367a88a");
        assert_eq!(m.display_name.as_deref(), Some("Some Movie"));
    }

    #[test]
    fn parses_base32_to_hex() {
        // Same infohash, base32-encoded.
        let m = parse("magnet:?xt=urn:btih:YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK").unwrap();
        assert_eq!(m.hash, "c12fe1c06bba254a9dc9f519b335aa7c1367a88a");
    }

    #[test]
    fn rejects_non_magnet() {
        assert!(parse("https://example.com").is_err());
    }
}
