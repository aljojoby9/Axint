//! Export Axiom history into a bare Git directory and publish it.
//!
//! `git` itself performs the network handshake, so GitHub authentication stays
//! in the user's existing SSH keys and credential helper. Axiom owns the
//! object translation.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use axiom_core::{
    AxiomHash, ChangeId, CommitDraft, Identity, ObjectStore, MODE_DIR, MODE_EXEC, MODE_FILE, MODE_SYMLINK,
};
use axiom_storage::{LeafInput, Repository};
use uuid::Uuid;

use crate::error::BridgeError;
use crate::object::{commit_body, encode_blob, encode_commit, encode_tree, tree_body, write_loose, GitOid};

const CHANGE_PREFIX: &str = "Change-Id:";
const GIT_PREFIX: &str = "Git-Oid:";

/// Pushes `bookmark` to a Git remote such as `git@github.com:org/repo.git`.
///
/// # Errors
///
/// Returns an error when `git` is missing, the bookmark has no commits, or the
/// remote rejects the update.
pub fn push_bookmark(repo: &Repository, bookmark: &str, url: &str) -> Result<GitOid, BridgeError> {
    let git_dir = bare_dir(repo);
    ensure_bare(&git_dir)?;
    let tip = export_bookmark(repo, bookmark, &git_dir)?;
    git(&git_dir, &["push", url, &format!("refs/heads/{bookmark}:refs/heads/{bookmark}")])?;
    Ok(tip)
}

/// Clones a Git URL into a new Axiom working tree at `dest`.
///
/// # Errors
///
/// Returns an error when `git clone` fails or a Git object cannot be imported.
pub fn clone_repository(url: &str, dest: &Path, name: &str, email: &str) -> Result<(), BridgeError> {
    if dest.exists() && dest.read_dir()?.next().is_some() {
        return Err(BridgeError::Message(format!("{} is not empty", dest.display())));
    }
    let mut repo = Repository::init(dest, name, email)?;
    let git_dir = bare_dir(&repo);
    fs::create_dir_all(git_dir.parent().unwrap_or(dest))?;
    let status = Command::new("git")
        .args(["clone", "--bare", url])
        .arg(&git_dir)
        .status()
        .map_err(|err| BridgeError::GitMissing(err.to_string()))?;
    if !status.success() {
        return Err(BridgeError::Git("git clone failed".to_owned()));
    }
    let head = git_text(&git_dir, &["rev-parse", "HEAD"])?;
    let tip = GitOid::from_hex(head.trim())?;
    let commits = git_text(&git_dir, &["rev-list", "--reverse", &tip.to_hex()])?;
    let mut map: HashMap<GitOid, AxiomHash> = HashMap::new();
    for line in commits.lines() {
        if line.is_empty() {
            continue;
        }
        let oid = GitOid::from_hex(line.trim())?;
        let axiom = import_commit(&mut repo, &git_dir, &oid, &map)?;
        map.insert(oid, axiom);
    }
    if let Some(axiom_tip) = map.get(&tip) {
        repo.checkout_commit(axiom_tip)?;
    }
    Ok(())
}

/// Writes every commit reachable from `bookmark` into `git_dir`.
///
/// # Errors
///
/// Returns an error when an Axiom object is missing or a loose object cannot
/// be written.
pub fn export_bookmark(repo: &Repository, bookmark: &str, git_dir: &Path) -> Result<GitOid, BridgeError> {
    let head = repo
        .bookmark(bookmark)?
        .ok_or_else(|| BridgeError::Message(format!("bookmark {bookmark} does not exist")))?;
    let mut commit_map: HashMap<AxiomHash, GitOid> = HashMap::new();
    let mut blob_map: HashMap<AxiomHash, GitOid> = HashMap::new();
    let ordered = oldest_first(repo, &head)?;
    for id in &ordered {
        export_one(repo, id, git_dir, &mut commit_map, &mut blob_map)?;
    }
    let tip = commit_map
        .get(&head)
        .copied()
        .ok_or_else(|| BridgeError::Message("export produced no tip".to_owned()))?;
    let ref_path = git_dir.join("refs").join("heads").join(bookmark);
    if let Some(parent) = ref_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&ref_path, format!("{tip}\n"))?;
    fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{bookmark}\n"))?;
    Ok(tip)
}

fn export_one(
    repo: &Repository,
    id: &AxiomHash,
    git_dir: &Path,
    commits: &mut HashMap<AxiomHash, GitOid>,
    blobs: &mut HashMap<AxiomHash, GitOid>,
) -> Result<(), BridgeError> {
    if commits.contains_key(id) {
        return Ok(());
    }
    let manifest = repo.get_commit(id)?;
    let tree_oid = export_tree(repo, &manifest.root()?, git_dir, blobs)?;
    let parents: Vec<GitOid> = manifest
        .parents()?
        .iter()
        .filter_map(|parent| commits.get(parent).copied())
        .collect();
    let author = signature_line(manifest.author.as_ref())?;
    let committer = signature_line(manifest.committer.as_ref())?;
    let message = with_change_id(&manifest.message, &manifest.change_id);
    let body = commit_body(&tree_oid, &parents, &author, &committer, &message);
    let (oid, raw) = encode_commit(&body);
    write_loose(git_dir, &oid, &raw)?;
    commits.insert(*id, oid);
    Ok(())
}

fn export_tree(
    repo: &Repository,
    tree: &AxiomHash,
    git_dir: &Path,
    blobs: &mut HashMap<AxiomHash, GitOid>,
) -> Result<GitOid, BridgeError> {
    let manifest = repo.get_tree(tree)?;
    let mut entries = Vec::new();
    for entry in manifest.entries {
        let mode = git_mode(entry.permissions, entry.kind()?);
        let oid = if entry.kind()? == axiom_core::EntryType::Directory {
            export_tree(repo, &entry.hash()?, git_dir, blobs)?
        } else {
            export_blob(repo, &entry.hash()?, git_dir, blobs)?
        };
        entries.push((mode, entry.name, oid));
    }
    let body = tree_body(&entries);
    let (oid, raw) = encode_tree(&body);
    write_loose(git_dir, &oid, &raw)?;
    Ok(oid)
}

fn export_blob(
    repo: &Repository,
    hash: &AxiomHash,
    git_dir: &Path,
    blobs: &mut HashMap<AxiomHash, GitOid>,
) -> Result<GitOid, BridgeError> {
    if let Some(oid) = blobs.get(hash) {
        return Ok(*oid);
    }
    let bytes = if let Ok(payload) = repo.read_file_bytes(hash) {
        payload
    } else if let Ok(conflict) = repo.get_conflict(hash) {
        format!("Axiom-Conflict\npath: {}\nkind: {}\n", conflict.path, conflict.kind).into_bytes()
    } else {
        return Err(BridgeError::Message(format!("cannot export object {hash}")));
    };
    let (oid, raw) = encode_blob(&bytes);
    write_loose(git_dir, &oid, &raw)?;
    blobs.insert(*hash, oid);
    Ok(oid)
}

fn import_commit(
    repo: &mut Repository,
    git_dir: &Path,
    oid: &GitOid,
    map: &HashMap<GitOid, AxiomHash>,
) -> Result<AxiomHash, BridgeError> {
    let text = git_text(git_dir, &["cat-file", "commit", &oid.to_hex()])?;
    let parsed = parse_commit(&text)?;
    let files = import_tree(repo, git_dir, &parsed.tree)?;
    let tree = repo.tree_from_files(&files)?;
    let parents = parsed
        .parents
        .iter()
        .filter_map(|parent| map.get(parent).copied())
        .collect();
    let author = identity_from_line(&parsed.author)?;
    let committer = identity_from_line(&parsed.committer)?;
    let change_id = change_id_from_message(&parsed.message, oid);
    let message = format!("{}\n{GIT_PREFIX} {oid}\n", parsed.message.trim_end());
    let draft = CommitDraft {
        root_tree_hash: tree,
        parents,
        change_id,
        sequence_number: 0,
        author,
        committer,
        message,
        conflict_records: Vec::new(),
    };
    Ok(repo.import_commit(draft, Some("main"))?)
}

fn import_tree(repo: &mut Repository, git_dir: &Path, tree: &GitOid) -> Result<BTreeMap<String, LeafInput>, BridgeError> {
    let listing = git_text(git_dir, &["ls-tree", "-r", &tree.to_hex()])?;
    let mut files = BTreeMap::new();
    for line in listing.lines() {
        if line.is_empty() {
            continue;
        }
        let (meta, path) = line
            .split_once('\t')
            .ok_or_else(|| BridgeError::Message(format!("bad ls-tree line: {line}")))?;
        let mut parts = meta.split_whitespace();
        let mode_text = parts
            .next()
            .ok_or_else(|| BridgeError::Message("ls-tree mode missing".to_owned()))?;
        let _kind = parts.next();
        let blob = parts
            .next()
            .ok_or_else(|| BridgeError::Message("ls-tree oid missing".to_owned()))?;
        let bytes = git_bytes(git_dir, &["cat-file", "blob", blob])?;
        let record = repo.ingest_bytes(&bytes)?;
        let mode = git_mode_to_axiom(mode_text);
        files.insert(
            path.replace('\\', "/"),
            LeafInput {
                manifest: record.manifest_hash,
                size: record.size,
                mode,
            },
        );
    }
    Ok(files)
}

struct ParsedCommit {
    tree: GitOid,
    parents: Vec<GitOid>,
    author: String,
    committer: String,
    message: String,
}

fn parse_commit(text: &str) -> Result<ParsedCommit, BridgeError> {
    let (header, message) = text.split_once("\n\n").unwrap_or((text, ""));
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = None;
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("tree ") {
            tree = Some(GitOid::from_hex(value)?);
        } else if let Some(value) = line.strip_prefix("parent ") {
            parents.push(GitOid::from_hex(value)?);
        } else if let Some(value) = line.strip_prefix("author ") {
            author = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("committer ") {
            committer = Some(value.to_owned());
        }
    }
    Ok(ParsedCommit {
        tree: tree.ok_or_else(|| BridgeError::Message("git commit has no tree".to_owned()))?,
        parents,
        author: author.ok_or_else(|| BridgeError::Message("git commit has no author".to_owned()))?,
        committer: committer.ok_or_else(|| BridgeError::Message("git commit has no committer".to_owned()))?,
        message: message.to_owned(),
    })
}

fn identity_from_line(line: &str) -> Result<Identity, BridgeError> {
    let (name_email, rest) = split_identity(line)?;
    let (name, email) = split_mailbox(name_email)?;
    let mut parts = rest.split_whitespace();
    let seconds: i64 = parts
        .next()
        .unwrap_or("0")
        .parse()
        .map_err(|_| BridgeError::Message("bad author timestamp".to_owned()))?;
    let zone = parts.next().unwrap_or("+0000");
    Identity::new(name, email, seconds, timezone_minutes(zone)).map_err(BridgeError::from)
}

fn split_identity(line: &str) -> Result<(&str, &str), BridgeError> {
    let marker = line
        .rfind('>')
        .ok_or_else(|| BridgeError::Message("identity is missing an email".to_owned()))?;
    let name_email = &line[..=marker];
    let rest = line[marker + 1..].trim();
    Ok((name_email, rest))
}

fn split_mailbox(text: &str) -> Result<(String, String), BridgeError> {
    let start = text
        .rfind('<')
        .ok_or_else(|| BridgeError::Message("identity is missing an email".to_owned()))?;
    let end = text
        .rfind('>')
        .ok_or_else(|| BridgeError::Message("identity is missing an email".to_owned()))?;
    let name = text[..start].trim().to_owned();
    let email = text[start + 1..end].trim().to_owned();
    Ok((name, email))
}

fn timezone_minutes(text: &str) -> i32 {
    let negative = text.starts_with('-');
    let digits = text.trim_start_matches(['+', '-']);
    let hours: i32 = digits.get(..2).unwrap_or("0").parse().unwrap_or(0);
    let mins: i32 = digits.get(2..4).unwrap_or("0").parse().unwrap_or(0);
    let total = hours.saturating_mul(60).saturating_add(mins);
    if negative {
        -total
    } else {
        total
    }
}

fn change_id_from_message(message: &str, oid: &GitOid) -> ChangeId {
    for line in message.lines() {
        if let Some(value) = line.trim().strip_prefix(CHANGE_PREFIX) {
            if let Ok(id) = ChangeId::parse(value.trim()) {
                return id;
            }
        }
    }
    let namespace = Uuid::new_v5(&Uuid::NAMESPACE_OID, b"axiom-git-bridge");
    ChangeId::from_uuid(Uuid::new_v5(&namespace, oid.as_bytes()))
}

fn with_change_id(message: &str, change_id: &str) -> String {
    if message.lines().any(|line| line.trim().starts_with(CHANGE_PREFIX)) {
        return message.to_owned();
    }
    let mut text = message.trim_end().to_owned();
    text.push_str(&format!("\n\n{CHANGE_PREFIX} {change_id}\n"));
    text
}

fn signature_line(signature: Option<&axiom_core::Signature>) -> Result<String, BridgeError> {
    let Some(signature) = signature else {
        return Err(BridgeError::Message("commit is missing a signature".to_owned()));
    };
    Ok(format!(
        "{} <{}> {} {}",
        signature.name,
        signature.email,
        signature.timestamp_seconds,
        format_zone(signature.timezone_offset_minutes)
    ))
}

fn format_zone(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let absolute = minutes.abs();
    format!("{sign}{:02}{:02}", absolute / 60, absolute % 60)
}

fn git_mode(mode: u32, kind: axiom_core::EntryType) -> u32 {
    if kind == axiom_core::EntryType::Directory || mode == MODE_DIR {
        0o040_000
    } else if mode == MODE_SYMLINK {
        0o120_000
    } else if mode == MODE_EXEC || kind == axiom_core::EntryType::Executable {
        0o100_755
    } else {
        let _ = MODE_FILE;
        0o100_644
    }
}

fn git_mode_to_axiom(text: &str) -> u32 {
    match text {
        "100755" => MODE_EXEC,
        "120000" => MODE_SYMLINK,
        "40000" | "040000" => MODE_DIR,
        _ => MODE_FILE,
    }
}

fn oldest_first(repo: &Repository, head: &AxiomHash) -> Result<Vec<AxiomHash>, BridgeError> {
    let mut ordered = Vec::new();
    let mut seen = HashMap::new();
    visit(repo, head, &mut ordered, &mut seen)?;
    Ok(ordered)
}

fn visit(
    repo: &Repository,
    id: &AxiomHash,
    ordered: &mut Vec<AxiomHash>,
    seen: &mut HashMap<AxiomHash, ()>,
) -> Result<(), BridgeError> {
    if seen.contains_key(id) {
        return Ok(());
    }
    seen.insert(*id, ());
    let manifest = repo.get_commit(id)?;
    for parent in manifest.parents()? {
        visit(repo, &parent, ordered, seen)?;
    }
    ordered.push(*id);
    Ok(())
}

fn bare_dir(repo: &Repository) -> PathBuf {
    repo.root().join(".axiom").join("git")
}

fn ensure_bare(git_dir: &Path) -> Result<(), BridgeError> {
    if git_dir.join("HEAD").is_file() {
        return Ok(());
    }
    if let Some(parent) = git_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("git")
        .args(["init", "--bare"])
        .arg(git_dir)
        .status()
        .map_err(|err| BridgeError::GitMissing(err.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(BridgeError::Git("git init --bare failed".to_owned()))
    }
}

fn git(git_dir: &Path, args: &[&str]) -> Result<String, BridgeError> {
    git_text(git_dir, args)
}

fn git_text(git_dir: &Path, args: &[&str]) -> Result<String, BridgeError> {
    let bytes = git_bytes(git_dir, args)?;
    String::from_utf8(bytes).map_err(|_| BridgeError::Message("git output is not utf-8".to_owned()))
}

fn git_bytes(git_dir: &Path, args: &[&str]) -> Result<Vec<u8>, BridgeError> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .map_err(|err| BridgeError::GitMissing(err.to_string()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(BridgeError::Git(String::from_utf8_lossy(&output.stderr).trim().to_owned()))
    }
}


