//! Git object codec.
//!
//! Loose objects use the standard `type size\\0payload` header, SHA-1, and
//! zlib. GitHub still addresses objects with SHA-1, so this bridge does too.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Digest, Sha1};

use crate::error::BridgeError;

/// A Git SHA-1 object id.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GitOid([u8; 20]);

impl GitOid {
    /// Wraps 20 raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Borrows the raw id.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// 40-digit hex.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    /// Parses 40 hex digits.
    ///
    /// # Errors
    ///
    /// Returns an error when `text` is not a SHA-1 hex digest.
    pub fn from_hex(text: &str) -> Result<Self, BridgeError> {
        let bytes = hex_decode(text.trim())?;
        let array: [u8; 20] = bytes
            .try_into()
            .map_err(|_| BridgeError::Message("git oid must be 20 bytes".to_owned()))?;
        Ok(Self(array))
    }
}

impl std::fmt::Display for GitOid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl std::fmt::Debug for GitOid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "GitOid({self})")
    }
}

/// One decoded Git object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitObject {
    /// `blob`, `tree`, or `commit`.
    pub kind: String,
    /// Payload after the header.
    pub payload: Vec<u8>,
}

/// Encodes a blob and returns its id plus the uncompressed object bytes.
#[must_use]
pub fn encode_blob(bytes: &[u8]) -> (GitOid, Vec<u8>) {
    encode_object("blob", bytes)
}

/// Encodes a tree body.
#[must_use]
pub fn encode_tree(body: &[u8]) -> (GitOid, Vec<u8>) {
    encode_object("tree", body)
}

/// Encodes a commit body.
#[must_use]
pub fn encode_commit(body: &[u8]) -> (GitOid, Vec<u8>) {
    encode_object("commit", body)
}

/// Builds a Git tree body. Directory modes must be `0o040000`.
///
/// Entries are sorted with Git's directory rule: a tree name sorts as `name/`.
#[must_use]
pub fn tree_body(entries: &[(u32, String, GitOid)]) -> Vec<u8> {
    let mut ordered = entries.to_vec();
    ordered.sort_by(|left, right| sort_key(left.0, &left.1).cmp(&sort_key(right.0, &right.1)));
    let mut body = Vec::new();
    for (mode, name, oid) in ordered {
        let mode_text = format!("{mode:o}");
        body.extend(mode_text.as_bytes());
        body.push(b' ');
        body.extend(name.as_bytes());
        body.push(0);
        body.extend(oid.as_bytes());
    }
    body
}

/// Formats a commit object body.
#[must_use]
pub fn commit_body(
    tree: &GitOid,
    parents: &[GitOid],
    author: &str,
    committer: &str,
    message: &str,
) -> Vec<u8> {
    let mut body = format!("tree {tree}\n");
    for parent in parents {
        body.push_str(&format!("parent {parent}\n"));
    }
    body.push_str(&format!("author {author}\ncommitter {committer}\n\n"));
    body.push_str(message);
    if !message.ends_with('\n') {
        body.push('\n');
    }
    body.into_bytes()
}

/// Decodes an uncompressed Git object.
///
/// # Errors
///
/// Returns an error when the header is malformed.
pub fn decode_object(bytes: &[u8]) -> Result<GitObject, BridgeError> {
    let nul = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| BridgeError::Message("git object is missing a header".to_owned()))?;
    let header = std::str::from_utf8(&bytes[..nul])
        .map_err(|_| BridgeError::Message("git object header is not utf-8".to_owned()))?;
    let (kind, size_text) = header
        .split_once(' ')
        .ok_or_else(|| BridgeError::Message("git object header has no size".to_owned()))?;
    let size: usize = size_text
        .parse()
        .map_err(|_| BridgeError::Message("git object size is invalid".to_owned()))?;
    let payload = bytes[nul + 1..].to_vec();
    if payload.len() != size {
        return Err(BridgeError::Message("git object size does not match the payload".to_owned()));
    }
    Ok(GitObject {
        kind: kind.to_owned(),
        payload,
    })
}

/// Writes a loose object under `git_dir/objects`.
///
/// # Errors
///
/// Returns an I/O error.
pub fn write_loose(git_dir: &Path, oid: &GitOid, object: &[u8]) -> Result<(), BridgeError> {
    let path = loose_path(git_dir, oid);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(object)?;
    let compressed = encoder.finish()?;
    let temporary = path.with_extension("tmp");
    {
        let mut file = File::create(&temporary)?;
        file.write_all(&compressed)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

/// Reads a loose object.
///
/// # Errors
///
/// Returns an error when the object is missing or the zlib stream is invalid.
pub fn read_loose(git_dir: &Path, oid: &GitOid) -> Result<GitObject, BridgeError> {
    let file = File::open(loose_path(git_dir, oid))?;
    let mut decoder = ZlibDecoder::new(file);
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    decode_object(&bytes)
}

fn encode_object(kind: &str, payload: &[u8]) -> (GitOid, Vec<u8>) {
    let header = format!("{kind} {}\0", payload.len());
    let mut raw = Vec::with_capacity(header.len() + payload.len());
    raw.extend(header.as_bytes());
    raw.extend(payload);
    let mut hasher = Sha1::new();
    hasher.update(&raw);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 20];
    bytes.copy_from_slice(&digest);
    (GitOid(bytes), raw)
}

fn sort_key(mode: u32, name: &str) -> String {
    if mode == 0o040_000 {
        format!("{name}/")
    } else {
        name.to_owned()
    }
}

fn loose_path(git_dir: &Path, oid: &GitOid) -> PathBuf {
    let hex = oid.to_hex();
    git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    text
}

fn hex_decode(text: &str) -> Result<Vec<u8>, BridgeError> {
    if !text.len().is_multiple_of(2) {
        return Err(BridgeError::Message("hex length is odd".to_owned()));
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let chars: Vec<char> = text.chars().collect();
    for pair in chars.chunks(2) {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_value(character: char) -> Result<u8, BridgeError> {
    match character {
        '0'..='9' => Ok(u8::try_from(character as u32).unwrap_or(0) - b'0'),
        'a'..='f' => Ok(u8::try_from(character as u32).unwrap_or(0) - b'a' + 10),
        'A'..='F' => Ok(u8::try_from(character as u32).unwrap_or(0) - b'A' + 10),
        _ => Err(BridgeError::Message("invalid hex digit".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_round_trip() -> Result<(), BridgeError> {
        let (oid, raw) = encode_blob(b"hello\n");
        let decoded = decode_object(&raw)?;
        assert_eq!(decoded.kind, "blob");
        assert_eq!(decoded.payload, b"hello\n");
        let dir = tempfile::tempdir().expect("tempdir");
        write_loose(dir.path(), &oid, &raw)?;
        assert_eq!(read_loose(dir.path(), &oid)?, decoded);
        assert_eq!(GitOid::from_hex(&oid.to_hex())?, oid);
        Ok(())
    }

    #[test]
    fn tree_sorts_directories_as_names_with_slash() {
        let blob = GitOid::from_bytes([1; 20]);
        let dir = GitOid::from_bytes([2; 20]);
        let body = tree_body(&[
            (0o100_644, "foo-bar".to_owned(), blob),
            (0o040_000, "foo".to_owned(), dir),
            (0o100_644, "foo.txt".to_owned(), blob),
        ]);
        let text = String::from_utf8_lossy(&body);
        let foo_dir = text.find("40000 foo\0").expect("dir entry");
        let foo_bar = text.find("100644 foo-bar\0").expect("file entry");
        let foo_txt = text.find("100644 foo.txt\0").expect("file entry");
        // Under Git's sorting rule, "foo" (dir) sorts as "foo/".
        // In ASCII, '-' (45) < '.' (46) < '/' (47), so foo-bar and foo.txt sort before foo/.
        // (Without the slash rule, "foo" prefix would incorrectly come first).
        assert!(foo_bar < foo_dir);
        assert!(foo_txt < foo_dir);
    }
}
