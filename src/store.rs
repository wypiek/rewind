//! On-disk history: content-addressed objects plus an append-only journal.
//!
//! Layout of `.rewind/`:
//!   journal.jsonl   one snapshot per line, each holding only what changed
//!   objects/ab/cd…  zstd-compressed file contents, named by blake3 hash
//!   index.json      stat cache (size, mtime) so scans only re-hash touched files
//!   lock            flock target so watchers and commands don't interleave

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const DIR: &str = ".rewind";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Change {
    /// 'A' added, 'M' modified, 'D' deleted
    pub op: char,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default)]
    pub mode: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub id: u64,
    pub time: i64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// For snapshots taken during `rewind run`: the snapshot the run started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u64>,
    /// For undo/goto/restore: the snapshot whose state was brought back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restores: Option<u64>,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub hash: String,
    pub mode: u32,
}

/// Path -> content of every tracked file at some point in time.
pub type State = BTreeMap<String, Entry>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub hash: String,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
}

pub type Index = BTreeMap<String, IndexEntry>;

pub fn to_state(idx: &Index) -> State {
    idx.iter()
        .map(|(p, e)| (p.clone(), Entry { hash: e.hash.clone(), mode: e.mode }))
        .collect()
}

pub fn apply(state: &mut State, s: &Snapshot) {
    for c in &s.changes {
        match &c.hash {
            Some(h) => {
                state.insert(c.path.clone(), Entry { hash: h.clone(), mode: c.mode });
            }
            None => {
                state.remove(&c.path);
            }
        }
    }
}

pub fn state_at(snaps: &[Snapshot], id: u64) -> State {
    let mut st = State::new();
    for s in snaps.iter().take_while(|s| s.id <= id) {
        apply(&mut st, s);
    }
    st
}

pub fn state_end(snaps: &[Snapshot]) -> State {
    state_at(snaps, u64::MAX)
}

pub fn diff_states(old: &State, new: &State) -> Vec<Change> {
    let mut out = Vec::new();
    for (p, e) in new {
        let op = match old.get(p) {
            None => 'A',
            Some(o) if o != e => 'M',
            _ => continue,
        };
        out.push(Change { op, path: p.clone(), hash: Some(e.hash.clone()), mode: e.mode });
    }
    for p in old.keys() {
        if !new.contains_key(p) {
            out.push(Change { op: 'D', path: p.clone(), hash: None, mode: 0 });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub fn find(snaps: &[Snapshot], id: u64) -> Option<&Snapshot> {
    snaps.binary_search_by_key(&id, |s| s.id).ok().map(|i| &snaps[i])
}

pub fn prev_id(snaps: &[Snapshot], id: u64) -> Option<u64> {
    snaps.iter().rev().find(|s| s.id < id).map(|s| s.id)
}

pub struct Repo {
    pub root: PathBuf,
    pub dir: PathBuf,
}

impl Repo {
    fn at(root: &Path) -> Repo {
        Repo { root: root.to_path_buf(), dir: root.join(DIR) }
    }

    pub fn find(start: &Path) -> Option<Repo> {
        start
            .ancestors()
            .find(|a| a.join(DIR).join("journal.jsonl").is_file())
            .map(Repo::at)
    }

    pub fn create(root: &Path) -> Result<Repo> {
        let r = Repo::at(root);
        fs::create_dir_all(r.dir.join("objects"))
            .with_context(|| format!("cannot create {}", r.dir.display()))?;
        fs::write(r.dir.join(".gitignore"), "# rewind's private history; never commit it\n*\n")?;
        OpenOptions::new().create(true).append(true).open(r.journal())?;
        Ok(r)
    }

    pub fn journal(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    /// Exclusive lock on the history. Released when the returned file drops.
    pub fn lock(&self) -> Result<File> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
        f.lock()?;
        Ok(f)
    }

    pub fn load(&self) -> Result<Vec<Snapshot>> {
        let f = File::open(self.journal()).context("cannot open rewind journal")?;
        let lines: Vec<String> = BufReader::new(f).lines().collect::<Result<_, _>>()?;
        let n = lines.len();
        let mut out = Vec::with_capacity(n);
        for (i, l) in lines.iter().enumerate() {
            if l.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Snapshot>(l) {
                Ok(s) => out.push(s),
                // A crash mid-append leaves a partial last line; skip it.
                Err(_) if i + 1 == n => {}
                Err(e) => bail!("journal line {} is corrupt: {e}", i + 1),
            }
        }
        Ok(out)
    }

    pub fn append(&self, s: &Snapshot) -> Result<()> {
        let mut f = OpenOptions::new().read(true).append(true).open(self.journal())?;
        let mut line = serde_json::to_string(s)?;
        line.push('\n');
        let len = f.metadata()?.len();
        if len > 0 {
            let mut last = [0u8; 1];
            f.seek(SeekFrom::Start(len - 1))?;
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.insert(0, '\n');
            }
        }
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
        Ok(())
    }

    pub fn rewrite(&self, snaps: &[Snapshot]) -> Result<()> {
        let tmp = self.dir.join("journal.jsonl.tmp");
        let mut out = String::new();
        for s in snaps {
            out.push_str(&serde_json::to_string(s)?);
            out.push('\n');
        }
        fs::write(&tmp, out)?;
        fs::rename(&tmp, self.journal())?;
        Ok(())
    }

    pub fn objects(&self) -> PathBuf {
        self.dir.join("objects")
    }

    fn obj_path(&self, h: &str) -> PathBuf {
        self.objects().join(&h[..2]).join(&h[2..])
    }

    pub fn put(&self, data: &[u8]) -> Result<String> {
        let h = blake3::hash(data).to_hex().to_string();
        let p = self.obj_path(&h);
        if !p.exists() {
            fs::create_dir_all(p.parent().unwrap())?;
            let comp = zstd::encode_all(data, 3)?;
            let tmp = p.with_extension(format!("tmp{}", std::process::id()));
            fs::write(&tmp, comp)?;
            fs::rename(&tmp, &p)?;
        }
        Ok(h)
    }

    pub fn get(&self, h: &str) -> Result<Vec<u8>> {
        let comp = fs::read(self.obj_path(h)).with_context(|| format!("missing object {h}"))?;
        Ok(zstd::decode_all(&comp[..])?)
    }

    pub fn load_index(&self) -> Index {
        fs::read(self.index_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_index(&self, idx: &Index) -> Result<()> {
        let tmp = self.dir.join(format!("index.json.tmp{}", std::process::id()));
        fs::write(&tmp, serde_json::to_vec(idx)?)?;
        fs::rename(&tmp, self.index_path())?;
        Ok(())
    }

    pub fn drop_index(&self) {
        let _ = fs::remove_file(self.index_path());
    }
}
