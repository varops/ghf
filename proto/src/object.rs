//! Content-addressed objects.
//!
//! An object's raw form is `<kind>\0<body>`; its id is the BLAKE3 hash of the
//! raw form, hex encoded. Objects are stored and transferred zstd-compressed.
//! A checkpoint's manifest is a tree of tree objects, structured like Git
//! trees, so unchanged directories are shared between checkpoints.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Blob,
    Tree,
}

impl Kind {
    fn tag(self) -> &'static [u8] {
        match self {
            Kind::Blob => b"blob\0",
            Kind::Tree => b"tree\0",
        }
    }
}

/// 32-byte BLAKE3 object id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Some(Self(out))
    }

    pub fn short(&self) -> String {
        self.to_hex()[..12].to_owned()
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.short())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for ObjectId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).ok_or_else(|| serde::de::Error::custom("invalid object id"))
    }
}

/// Build the raw (uncompressed) form of an object.
pub fn raw(kind: Kind, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 5);
    v.extend_from_slice(kind.tag());
    v.extend_from_slice(body);
    v
}

pub fn hash_raw(raw: &[u8]) -> ObjectId {
    ObjectId(*blake3::hash(raw).as_bytes())
}

pub fn hash(kind: Kind, body: &[u8]) -> ObjectId {
    let mut h = blake3::Hasher::new();
    h.update(kind.tag());
    h.update(body);
    ObjectId(*h.finalize().as_bytes())
}

/// Split a raw object into kind and body.
pub fn parse_raw(raw: &[u8]) -> Option<(Kind, &[u8])> {
    if let Some(b) = raw.strip_prefix(Kind::Blob.tag()) {
        Some((Kind::Blob, b))
    } else {
        raw.strip_prefix(Kind::Tree.tag()).map(|b| (Kind::Tree, b))
    }
}

pub fn compress(raw: &[u8]) -> Vec<u8> {
    zstd::encode_all(raw, 3).expect("zstd encode to memory")
}

pub fn decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::decode_all(data)
}

/// Decompress and verify that `data` is the object `id`. Returns the raw form.
pub fn verify(id: &ObjectId, compressed: &[u8]) -> Option<Vec<u8>> {
    let raw = decompress(compressed).ok()?;
    (hash_raw(&raw) == *id && parse_raw(&raw).is_some()).then_some(raw)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    File,
    Exec,
    Symlink,
    Tree,
    /// A submodule: the blob holds the checked-out commit id, as hex text.
    Gitlink,
}

impl Mode {
    /// The Git index mode for this entry kind.
    pub fn git_mode(self) -> &'static str {
        match self {
            Mode::File => "100644",
            Mode::Exec => "100755",
            Mode::Symlink => "120000",
            Mode::Tree => "040000",
            Mode::Gitlink => "160000",
        }
    }

    pub fn from_git_mode(s: &str) -> Self {
        match s {
            "100755" => Mode::Exec,
            "120000" => Mode::Symlink,
            "160000" => Mode::Gitlink,
            "040000" => Mode::Tree,
            _ => Mode::File,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Mode::File => "f",
            Mode::Exec => "x",
            Mode::Symlink => "l",
            Mode::Tree => "t",
            Mode::Gitlink => "g",
        }
    }

    fn from_code(s: &str) -> Option<Self> {
        Some(match s {
            "f" => Mode::File,
            "x" => Mode::Exec,
            "l" => Mode::Symlink,
            "t" => Mode::Tree,
            "g" => Mode::Gitlink,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub mode: Mode,
    pub id: ObjectId,
    /// File size in bytes (blobs) or 0 (trees).
    pub size: u64,
}

/// Encode tree entries. Entries are sorted by name; names must be unique and
/// contain neither `/` nor NUL.
pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted: Vec<&TreeEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut out = Vec::new();
    for e in sorted {
        out.extend_from_slice(
            format!(
                "{} {} {}\t{}\0",
                e.mode.code(),
                e.size,
                e.id.to_hex(),
                e.name
            )
            .as_bytes(),
        );
    }
    out
}

/// Is `name` safe as one path component in a worktree? Rejects empty
/// names, `.`, `..`, `/`, NUL and `.git` (any case), so a manifest
/// can never address anything outside its worktree or inside Git's own dir.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\0'])
        && !name.eq_ignore_ascii_case(".git")
}

/// Decode a tree. Returns `None` for malformed trees, including unsafe or
/// duplicate entry names.
pub fn decode_tree(body: &[u8]) -> Option<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut names = std::collections::HashSet::new();
    for rec in body.split(|b| *b == 0) {
        if rec.is_empty() {
            continue;
        }
        let rec = std::str::from_utf8(rec).ok()?;
        let (head, name) = rec.split_once('\t')?;
        let mut parts = head.split(' ');
        let mode = Mode::from_code(parts.next()?)?;
        let size = parts.next()?.parse().ok()?;
        let id = ObjectId::from_hex(parts.next()?)?;
        if !safe_name(name) || !names.insert(name) {
            return None;
        }
        entries.push(TreeEntry {
            name: name.to_owned(),
            mode,
            id,
            size,
        });
    }
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_round_trip_is_order_independent() {
        let blob = hash(Kind::Blob, b"hi");
        let a = TreeEntry {
            name: "b.txt".into(),
            mode: Mode::File,
            id: blob,
            size: 2,
        };
        let b = TreeEntry {
            name: "a dir\twith tab".into(),
            mode: Mode::Exec,
            id: blob,
            size: 2,
        };
        let one = encode_tree(&[a.clone(), b.clone()]);
        let two = encode_tree(&[b.clone(), a.clone()]);
        assert_eq!(one, two);
        assert_eq!(decode_tree(&one).unwrap(), vec![b, a]);
    }

    #[test]
    fn unsafe_names_are_rejected() {
        let id = hash(Kind::Blob, b"x");
        for bad in ["..", ".", "", ".git", ".GIT", "a/b"] {
            let body = format!("f 1 {}\t{bad}\0", id.to_hex());
            assert!(decode_tree(body.as_bytes()).is_none(), "{bad:?} accepted");
        }
        let dup = format!("f 1 {0}\tx\0f 1 {0}\tx\0", id.to_hex());
        assert!(decode_tree(dup.as_bytes()).is_none());
        assert!(decode_tree(format!("f 1 {}\t.gitignore\0", id.to_hex()).as_bytes()).is_some());
    }

    #[test]
    fn verify_checks_hash() {
        let r = raw(Kind::Blob, b"content");
        let id = hash_raw(&r);
        assert_eq!(id, hash(Kind::Blob, b"content"));
        let c = compress(&r);
        assert!(verify(&id, &c).is_some());
        assert!(verify(&hash(Kind::Blob, b"other"), &c).is_none());
    }
}
