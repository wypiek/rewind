mod scan;
mod store;
mod ui;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use scan::scan;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use store::*;
use ui::*;

const QUICKSTART: &str = "\
Quick start:
  rewind init                 start tracking this folder
  rewind run -- claude        run an AI agent; everything it touches is recorded
  rewind undo                 take back the last change-set (a whole agent run is one)
  rewind watch                (spare terminal) snapshot every save automatically
  rewind log                  timeline of snapshots
  rewind diff 12              compare snapshot #12 with now
  rewind restore src/app.ts   put one file back to before its last change

Snapshot refs: 12 (number), last, -1 (one before last), 10m / 2h / 3d (time ago).
Honors .gitignore; add a .rewindignore (gitignore syntax) for extra rules,
e.g. `!.env` to also keep history of your .env file.";

#[derive(Parser)]
#[command(
    name = "rewind",
    version,
    about = "A time machine for your project folder: every save and every AI edit, undoable.",
    after_help = QUICKSTART
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start tracking the current folder
    Init {
        /// Allow huge folders and folders inside another rewind project
        #[arg(long)]
        force: bool,
    },
    /// Save a checkpoint now
    Snap {
        /// Name for the checkpoint, e.g. "before refactor"
        #[arg(short, long)]
        message: Option<String>,
        /// Print nothing (for hooks and scripts)
        #[arg(short, long)]
        quiet: bool,
    },
    /// Stay running and snapshot automatically whenever files change
    Watch {
        /// How often to look for changes, in milliseconds
        #[arg(long, default_value_t = 1000)]
        interval: u64,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Run a command (an AI agent, a codemod, a script) and record everything it changes
    Run {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// What changed since the last snapshot
    Status,
    /// Timeline of snapshots, newest first
    Log {
        /// Only snapshots that touched this file or folder
        path: Option<String>,
        #[arg(short = 'n', default_value_t = 25)]
        count: usize,
        #[arg(long)]
        all: bool,
    },
    /// Line diff: no ref = last snapshot vs now; one ref = it vs now; two = between them
    Diff {
        #[arg(allow_negative_numbers = true)]
        refs: Vec<String>,
        /// Only list files with line counts
        #[arg(long)]
        stat: bool,
        /// Limit to these files or folders (after --)
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// What one snapshot changed
    Show {
        #[arg(default_value = "last", allow_negative_numbers = true)]
        r#ref: String,
        #[arg(long)]
        stat: bool,
    },
    /// Take back the most recent change-set; repeat to go further back
    Undo,
    /// Put files or folders back (default: to before their last change)
    Restore {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Restore to this snapshot instead
        #[arg(long, short, allow_negative_numbers = true)]
        at: Option<String>,
    },
    /// Move the whole project to a snapshot (current state is saved first)
    Goto {
        #[arg(allow_negative_numbers = true)]
        r#ref: String,
    },
    /// Print a file as it was in a snapshot
    Cat {
        path: String,
        #[arg(long, short, allow_negative_numbers = true)]
        at: Option<String>,
    },
    /// Drop history older than a duration to save space
    Prune {
        #[arg(long, default_value = "14d")]
        older_than: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let res = match cli.cmd {
        Cmd::Init { force } => cmd_init(force),
        Cmd::Snap { message, quiet } => cmd_snap(message, quiet),
        Cmd::Watch { interval, quiet } => cmd_watch(interval, quiet),
        Cmd::Run { command } => cmd_run(command),
        Cmd::Status => cmd_status(),
        Cmd::Log { path, count, all } => cmd_log(path, count, all),
        Cmd::Diff { refs, stat, paths } => cmd_diff(refs, stat, paths),
        Cmd::Show { r#ref, stat } => cmd_show(&r#ref, stat),
        Cmd::Undo => cmd_undo(),
        Cmd::Restore { paths, at } => cmd_restore(paths, at),
        Cmd::Goto { r#ref } => cmd_goto(&r#ref),
        Cmd::Cat { path, at } => cmd_cat(&path, at),
        Cmd::Prune { older_than } => cmd_prune(&older_than),
    };
    if let Err(e) = res {
        eprintln!("{} {e:#}", red("rewind:"));
        std::process::exit(1);
    }
}

// ---------- helpers ----------

const MAX_INIT_FILES: usize = 20_000;
const SETTLE: Duration = Duration::from_millis(1500);
const MAX_WAIT: Duration = Duration::from_secs(10);

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn trap_signals(term: bool) {
    let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, h);
        if term {
            libc::signal(libc::SIGTERM, h);
        }
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn open(auto_init: bool) -> Result<Repo> {
    let cwd = env::current_dir()?;
    if let Some(r) = Repo::find(&cwd) {
        return Ok(r);
    }
    if !auto_init {
        bail!("not inside a rewind project (no .rewind folder here or above).\n  start one with: rewind init");
    }
    guard_dir(&cwd)?;
    init_at(&cwd, false)
}

fn guard_dir(dir: &Path) -> Result<()> {
    let home = env::var_os("HOME").map(PathBuf::from);
    if dir == Path::new("/") || home.as_deref() == Some(dir) {
        bail!(
            "refusing to track {} as one project; cd into a project folder first (or `rewind init --force`)",
            dir.display()
        );
    }
    Ok(())
}

fn init_at(root: &Path, force: bool) -> Result<Repo> {
    let existed = root.join(DIR).exists();
    let repo = Repo::create(root)?;
    let sc = match scan(&repo, &Index::new(), true, (!force).then_some(MAX_INIT_FILES)) {
        Ok(s) => s,
        Err(e) => {
            if !existed {
                let _ = fs::remove_dir_all(&repo.dir);
            }
            return Err(e);
        }
    };
    let (_, idx) = snapshot(&repo, "init", "", None, None, Some(&sc.index), true)?;
    let bytes: u64 = idx.values().map(|e| e.size).sum();
    eprintln!(
        "{} tracking {} files ({}) in {}",
        green("rewind:"),
        idx.len(),
        human(bytes),
        root.display()
    );
    if !sc.skipped.is_empty() {
        eprintln!("{}", dim(&format!("  skipped {} files over 20 MB", sc.skipped.len())));
    }
    Ok(repo)
}

/// Lock, scan, and record whatever changed since the last snapshot.
fn snapshot(
    repo: &Repo,
    kind: &str,
    label: &str,
    group: Option<u64>,
    restores: Option<u64>,
    prev: Option<&Index>,
    force: bool,
) -> Result<(Option<Snapshot>, Index)> {
    let _lock = repo.lock()?;
    snapshot_locked(repo, kind, label, group, restores, prev, force)
}

fn snapshot_locked(
    repo: &Repo,
    kind: &str,
    label: &str,
    group: Option<u64>,
    restores: Option<u64>,
    prev: Option<&Index>,
    force: bool,
) -> Result<(Option<Snapshot>, Index)> {
    let snaps = repo.load()?;
    let loaded;
    let prev = match prev {
        Some(p) => p,
        None => {
            loaded = repo.load_index();
            &loaded
        }
    };
    let sc = scan(repo, prev, true, None)?;
    repo.save_index(&sc.index)?;
    let changes = diff_states(&state_end(&snaps), &to_state(&sc.index));
    if changes.is_empty() && !force {
        return Ok((None, sc.index));
    }
    let s = Snapshot {
        id: snaps.last().map_or(1, |s| s.id + 1),
        time: now(),
        kind: kind.into(),
        label: label.into(),
        group,
        restores,
        changes,
    };
    repo.append(&s)?;
    Ok((Some(s), sc.index))
}

/// The working tree as it is right now, without writing anything to history.
fn working_state(repo: &Repo) -> Result<State> {
    Ok(to_state(&scan(repo, &repo.load_index(), false, None)?.index))
}

fn parse_dur(s: &str) -> Option<i64> {
    let s = s.trim();
    let unit = s.chars().last()?;
    let n: i64 = s[..s.len() - unit.len_utf8()].parse().ok()?;
    let mul = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        'w' => 604800,
        _ => return None,
    };
    Some(n * mul)
}

fn resolve(snaps: &[Snapshot], r: &str) -> Result<u64> {
    let r = r.trim().trim_start_matches('#');
    if snaps.is_empty() {
        bail!("no snapshots yet");
    }
    if let Ok(id) = r.parse::<u64>() {
        return find(snaps, id).map(|s| s.id).ok_or_else(|| anyhow!("no snapshot #{id} (see `rewind log`)"));
    }
    if r == "last" {
        return Ok(snaps.last().unwrap().id);
    }
    if let Some(n) = r.strip_prefix('-') {
        let n: usize = n.parse().map_err(|_| anyhow!("bad ref -{n}"))?;
        return snaps
            .iter()
            .rev()
            .nth(n)
            .map(|s| s.id)
            .ok_or_else(|| anyhow!("only {} snapshots exist", snaps.len()));
    }
    if let Some(d) = parse_dur(r) {
        let t = now() - d;
        return snaps
            .iter()
            .rev()
            .find(|s| s.time <= t)
            .map(|s| s.id)
            .ok_or_else(|| anyhow!("no snapshot is older than {r}"));
    }
    bail!("don't understand `{r}`: use a snapshot number (12), steps back (-2), `last`, or a time ago (10m, 2h, 3d)")
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Turn a path typed by the user (relative to cwd) into a project-relative path.
fn rel_path(repo: &Repo, p: &str) -> Result<String> {
    let abs = normalize(&env::current_dir()?.join(p));
    let rel = abs
        .strip_prefix(&repo.root)
        .map_err(|_| anyhow!("{p} is outside the project ({})", repo.root.display()))?;
    Ok(rel.to_string_lossy().trim_end_matches('/').to_string())
}

fn under(path: &str, filter: &str) -> bool {
    filter.is_empty()
        || path == filter
        || (path.starts_with(filter) && path.as_bytes().get(filter.len()) == Some(&b'/'))
}

fn matches(path: &str, filters: &[String]) -> bool {
    filters.is_empty() || filters.iter().any(|f| under(path, f))
}

enum Side<'a> {
    Snap(&'a State),
    Disk,
}

impl Side<'_> {
    fn read(&self, repo: &Repo, path: &str) -> Option<Vec<u8>> {
        match self {
            Side::Snap(st) => st.get(path).and_then(|e| repo.get(&e.hash).ok()),
            Side::Disk => fs::read(repo.root.join(path)).ok(),
        }
    }
}

fn counts(ch: &[Change]) -> String {
    let n = |op| ch.iter().filter(|c| c.op == op).count();
    let mut parts = Vec::new();
    let (a, m, d) = (n('A'), n('M'), n('D'));
    if a > 0 {
        parts.push(green(&format!("+{a}")));
    }
    if m > 0 {
        parts.push(yellow(&format!("~{m}")));
    }
    if d > 0 {
        parts.push(red(&format!("-{d}")));
    }
    if parts.is_empty() { dim("(checkpoint)") } else { parts.join(" ") }
}

fn preview(ch: &[Change], max: usize) -> String {
    let mut s: Vec<&str> = ch.iter().take(max).map(|c| c.path.as_str()).collect();
    let more = ch.len().saturating_sub(max);
    let more = if more > 0 { format!(" +{more} more") } else { String::new() };
    s.dedup();
    dim(&format!("{}{more}", s.join(", ")))
}

fn change_lines(repo: &Repo, old: &Side, new: &Side, ch: &[Change]) -> String {
    let mut out = String::new();
    for c in ch {
        let (sym, code) = match c.op {
            'A' => ("+", "32"),
            'D' => ("-", "31"),
            _ => ("~", "33"),
        };
        let a = old.read(repo, &c.path).unwrap_or_default();
        let b = new.read(repo, &c.path).unwrap_or_default();
        let st = match line_stats(&a, &b) {
            Some((i, d)) => format!("{} {}", green(&format!("+{i}")), red(&format!("-{d}"))),
            None => dim("binary"),
        };
        out.push_str(&format!("  {} {}  {}\n", paint(code, sym), c.path, st));
    }
    out
}

fn short(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max { s } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

fn snap_line(s: &Snapshot) -> String {
    format!(
        "{}  {}  {}  {}",
        yellow(&format!("#{}", s.id)),
        dim(&clock(s.time)),
        counts(&s.changes),
        preview(&s.changes, 3)
    )
}

/// Write `want` to disk, given that disk currently matches `cur`.
/// Files last modified before `since` are never deleted: they existed back then and were
/// only hidden by ignore rules (e.g. an agent deleted .gitignore and .env became visible).
fn apply_state(repo: &Repo, cur: &State, want: &State, since: i64) -> Result<Vec<Change>> {
    let mut ch = diff_states(cur, want);
    ch.retain(|c| {
        let old = c.op == 'D'
            && fs::metadata(repo.root.join(&c.path)).is_ok_and(|m| m.mtime() < since);
        if old {
            eprintln!(
                "{} kept {}: it is older than the snapshot, so it was ignored then, not missing",
                yellow("note:"),
                c.path
            );
        }
        !old
    });
    for c in ch.iter().filter(|c| c.op == 'D') {
        let p = repo.root.join(&c.path);
        if let Err(e) = fs::remove_file(&p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("{} could not delete {}: {e}", yellow("warning:"), c.path);
        }
        let mut dir = p.parent();
        while let Some(d) = dir {
            if d == repo.root || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    for c in ch.iter().filter(|c| c.op != 'D') {
        let data = repo.get(c.hash.as_deref().unwrap())?;
        let p = repo.root.join(&c.path);
        if p.is_dir() {
            eprintln!("{} {} is a folder now; left it alone", yellow("warning:"), c.path);
            continue;
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        let name = p.file_name().unwrap().to_string_lossy();
        let tmp = p.with_file_name(format!(".{name}.rewind-tmp"));
        fs::write(&tmp, &data)?;
        if c.mode != 0 {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(c.mode))?;
        }
        fs::rename(&tmp, &p)?;
    }
    Ok(ch)
}

/// Polls the tree and records a snapshot once changes settle down.
struct Watcher {
    idx: Index,
    dirty: bool,
    first_change: Instant,
    last_change: Instant,
}

impl Watcher {
    fn new(idx: Index) -> Self {
        Watcher { idx, dirty: false, first_change: Instant::now(), last_change: Instant::now() }
    }

    fn tick(&mut self, repo: &Repo, kind: &str, label: &str, group: Option<u64>) -> Result<Option<Snapshot>> {
        let sc = scan(repo, &self.idx, true, None)?;
        let t = Instant::now();
        if sc.index != self.idx {
            if !self.dirty {
                self.first_change = t;
            }
            self.idx = sc.index;
            self.dirty = true;
            self.last_change = t;
        }
        let settled = t.duration_since(self.last_change) >= SETTLE;
        let overdue = t.duration_since(self.first_change) >= MAX_WAIT;
        if self.dirty && (settled || overdue) {
            return self.flush(repo, kind, label, group);
        }
        Ok(None)
    }

    fn flush(&mut self, repo: &Repo, kind: &str, label: &str, group: Option<u64>) -> Result<Option<Snapshot>> {
        let (s, idx) = snapshot(repo, kind, label, group, None, Some(&self.idx), false)?;
        self.idx = idx;
        self.dirty = false;
        Ok(s)
    }
}

/// Walk back past undo snapshots to the state the project is effectively at.
fn effective_pos(snaps: &[Snapshot], mut pos: u64) -> u64 {
    while let Some(s) = find(snaps, pos) {
        match s.restores {
            Some(r) if s.kind == "undo" && r < pos => pos = r,
            _ => break,
        }
    }
    pos
}

/// Where undoing the change-set that ends at `pos` would take us.
fn changeset_start(snaps: &[Snapshot], pos: u64) -> Option<u64> {
    let s = find(snaps, pos)?;
    if let Some(g) = s.group
        && find(snaps, g).is_some()
    {
        return Some(g);
    }
    prev_id(snaps, pos)
}

// ---------- commands ----------

fn cmd_init(force: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    if let Some(r) = Repo::find(&cwd) {
        if r.root == cwd {
            println!("already tracking {}", cwd.display());
            return Ok(());
        }
        if !force {
            bail!(
                "{} is already inside a rewind project at {} (use --force to nest)",
                cwd.display(),
                r.root.display()
            );
        }
    }
    if !force {
        guard_dir(&cwd)?;
    }
    init_at(&cwd, force)?;
    eprintln!("{}", dim("  next: `rewind run -- <your agent>` or `rewind watch` in a spare terminal"));
    Ok(())
}

fn cmd_snap(message: Option<String>, quiet: bool) -> Result<()> {
    let repo = open(true)?;
    let label = message.clone().unwrap_or_default();
    let (s, _) = snapshot(&repo, "snap", &label, None, None, None, message.is_some() && !quiet)?;
    if quiet {
        return Ok(());
    }
    match s {
        Some(s) => println!("{}", snap_line(&s)),
        None => println!("nothing changed since the last snapshot"),
    }
    Ok(())
}

fn cmd_watch(interval: u64, quiet: bool) -> Result<()> {
    let repo = open(true)?;
    let (s, idx) = snapshot(&repo, "auto", "", None, None, None, false)?;
    if !quiet {
        eprintln!(
            "{} watching {}; every change becomes a snapshot. Ctrl-C to stop.",
            green("rewind:"),
            repo.root.display()
        );
        if let Some(s) = s {
            println!("{}", snap_line(&s));
        }
    }
    trap_signals(true);
    let mut w = Watcher::new(idx);
    let step = Duration::from_millis(interval.clamp(100, 60_000));
    while !STOP.load(Ordering::SeqCst) {
        let start = Instant::now();
        while start.elapsed() < step && !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
        match w.tick(&repo, "auto", "", None) {
            Ok(Some(s)) if !quiet => println!("{}", snap_line(&s)),
            Ok(_) => {}
            Err(e) => eprintln!("{} {e:#}", yellow("warning:")),
        }
    }
    if let Some(s) = w.flush(&repo, "auto", "", None)?
        && !quiet
    {
        println!("{}", snap_line(&s));
    }
    Ok(())
}

fn cmd_run(argv: Vec<String>) -> Result<()> {
    let repo = open(true)?;
    let cmdline = argv.join(" ");
    let shown = short(&cmdline, 50);
    let (_, idx) = snapshot(&repo, "save", &format!("before: {cmdline}"), None, None, None, false)?;
    let base = repo.load()?.last().map(|s| s.id).ok_or_else(|| anyhow!("empty history"))?;
    eprintln!(
        "{} recording `{shown}` from #{base}. Undo it all later with `rewind undo`.",
        green("rewind:")
    );

    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .map_err(|e| anyhow!("cannot start `{}`: {e}", argv[0]))?;
    // Ctrl-C goes to the child too; we just keep recording until it exits.
    trap_signals(false);

    let mut w = Watcher::new(idx);
    let mut last_tick = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        std::thread::sleep(Duration::from_millis(200));
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            if let Err(e) = w.tick(&repo, "auto", &cmdline, Some(base)) {
                eprintln!("{} {e:#}", yellow("warning:"));
            }
        }
    };
    snapshot(&repo, "run", &cmdline, Some(base), None, Some(&w.idx), false)?;

    let snaps = repo.load()?;
    let before = state_at(&snaps, base);
    let after = state_end(&snaps);
    let ch = diff_states(&before, &after);
    eprintln!();
    if ch.is_empty() {
        eprintln!("{} `{shown}` changed no files.", green("rewind:"));
    } else {
        eprintln!("{} `{shown}` changed {} files:", green("rewind:"), ch.len());
        eprint!("{}", change_lines(&repo, &Side::Snap(&before), &Side::Snap(&after), &ch));
        eprintln!("{}", dim(&format!("  review: rewind diff {base}    take it all back: rewind undo")));
    }
    let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    std::process::exit(code);
}

fn cmd_status() -> Result<()> {
    let repo = open(false)?;
    let snaps = repo.load()?;
    let work = working_state(&repo)?;
    let cur = state_end(&snaps);
    let ch = diff_states(&cur, &work);
    let size = dir_size(&repo.objects());
    let t = now();
    println!("{}  {}", bold(&repo.root.display().to_string()), dim(&format!("{} files tracked", work.len())));
    if let Some(last) = snaps.last() {
        println!(
            "{}",
            dim(&format!(
                "{} snapshots, history uses {}; last is #{} from {}",
                snaps.len(),
                human(size),
                last.id,
                ago(last.time, t)
            ))
        );
    }
    if ch.is_empty() {
        println!("no changes since the last snapshot");
    } else {
        println!("changes not in any snapshot yet:");
        print!("{}", change_lines(&repo, &Side::Snap(&cur), &Side::Disk, &ch));
        println!("{}", dim("  keep: rewind snap    see: rewind diff    throw away: rewind undo"));
    }
    Ok(())
}

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(p) else { return 0 };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

fn cmd_log(path: Option<String>, count: usize, all: bool) -> Result<()> {
    let repo = open(false)?;
    let snaps = repo.load()?;
    let filter: Vec<String> = path.map(|p| rel_path(&repo, &p)).transpose()?.into_iter().collect();
    let t = now();
    let hits: Vec<&Snapshot> = snaps
        .iter()
        .rev()
        .filter(|s| filter.is_empty() || s.changes.iter().any(|c| matches(&c.path, &filter)))
        .collect();
    let shown = if all { hits.len() } else { count.min(hits.len()) };
    let mut out = String::new();
    for s in &hits[..shown] {
        let ch: Vec<Change> = s.changes.iter().filter(|c| matches(&c.path, &filter)).cloned().collect();
        let mut label = s.label.clone();
        if let Some(r) = s.restores
            && label.is_empty()
        {
            label = format!("back to #{r}");
        }
        if s.kind == "auto"
            && let Some(g) = s.group
        {
            label = format!("during run from #{g}: {label}");
        }
        let label = if label.is_empty() { String::new() } else { format!("{}  ", bold(&short(&label, 48))) };
        out.push_str(&format!(
            "{}  {}  {}  {} {}{}  {}\n",
            yellow(&format!("{:>5}", format!("#{}", s.id))),
            dim(&format!("{:>12}", clock(s.time))),
            dim(&format!("{:>7}", ago(s.time, t))),
            format!("{:<7}", s.kind),
            label,
            counts(&ch),
            preview(&ch, 3)
        ));
    }
    if shown < hits.len() {
        out.push_str(&dim(&format!("  … {} older (use --all)\n", hits.len() - shown)));
    }
    if hits.is_empty() {
        out.push_str("no snapshots touch that path\n");
    }
    page(&out);
    Ok(())
}

fn cmd_diff(refs: Vec<String>, stat: bool, paths: Vec<String>) -> Result<()> {
    let repo = open(false)?;
    let snaps = repo.load()?;
    let filter: Vec<String> = paths.iter().map(|p| rel_path(&repo, p)).collect::<Result<_>>()?;
    let (old_id, new_id) = match refs.as_slice() {
        [] => (snaps.last().map(|s| s.id).ok_or_else(|| anyhow!("no snapshots yet"))?, None),
        [a] => (resolve(&snaps, a)?, None),
        [a, b] => (resolve(&snaps, a)?, Some(resolve(&snaps, b)?)),
        _ => bail!("give at most two snapshot refs (put paths after --)"),
    };
    let old = state_at(&snaps, old_id);
    let new = match new_id {
        Some(id) => state_at(&snaps, id),
        None => working_state(&repo)?,
    };
    let ch: Vec<Change> = diff_states(&old, &new).into_iter().filter(|c| matches(&c.path, &filter)).collect();
    let (os, ns) = (Side::Snap(&old), if new_id.is_some() { Side::Snap(&new) } else { Side::Disk });
    print_diff(&repo, &os, &ns, &ch, stat);
    Ok(())
}

fn print_diff(repo: &Repo, old: &Side, new: &Side, ch: &[Change], stat: bool) {
    if ch.is_empty() {
        println!("no differences");
        return;
    }
    if stat {
        page(&change_lines(repo, old, new, ch));
        return;
    }
    let mut out = String::new();
    for c in ch {
        let a = if c.op == 'A' { None } else { old.read(repo, &c.path) };
        let b = if c.op == 'D' { None } else { new.read(repo, &c.path) };
        out.push_str(&unified(&c.path, a.as_deref(), b.as_deref()));
    }
    page(&out);
}

fn cmd_show(r: &str, stat: bool) -> Result<()> {
    let repo = open(false)?;
    let snaps = repo.load()?;
    let id = resolve(&snaps, r)?;
    let s = find(&snaps, id).unwrap();
    let old = prev_id(&snaps, id).map(|p| state_at(&snaps, p)).unwrap_or_default();
    let new = state_at(&snaps, id);
    println!(
        "{} {} {} {}",
        yellow(&format!("#{id}")),
        s.kind,
        bold(&s.label),
        dim(&format!("{} ({})", clock(s.time), ago(s.time, now())))
    );
    print_diff(&repo, &Side::Snap(&old), &Side::Snap(&new), &s.changes, stat);
    Ok(())
}

fn cmd_undo() -> Result<()> {
    let repo = open(false)?;
    let _lock = repo.lock()?;
    snapshot_locked(&repo, "save", "unsaved changes (before undo)", None, None, None, false)?;
    let snaps = repo.load()?;
    let last = snaps.last().ok_or_else(|| anyhow!("no snapshots yet"))?.id;

    // Find the most recent change-set that actually changed something.
    let cur = state_end(&snaps);
    let mut pos = effective_pos(&snaps, last);
    let target = loop {
        let Some(t) = changeset_start(&snaps, pos) else {
            bail!("nothing left to undo: #{pos} is where history begins");
        };
        if state_at(&snaps, t) != state_at(&snaps, pos) {
            break t;
        }
        pos = effective_pos(&snaps, t);
    };
    let undone = find(&snaps, pos).unwrap();
    let what = if undone.label.is_empty() { undone.kind.clone() } else { short(&undone.label, 60) };

    let want = state_at(&snaps, target);
    let ch = apply_state(&repo, &cur, &want, find(&snaps, target).unwrap().time)?;
    snapshot_locked(&repo, "undo", &format!("undid #{pos} ({what})"), None, Some(target), None, true)?;

    println!("{} undid #{pos} {} — project now matches #{target}", green("rewind:"), bold(&what));
    print!("{}", change_lines(&repo, &Side::Snap(&cur), &Side::Snap(&want), &ch));
    println!("{}", dim(&format!("  changed your mind? rewind goto {last}    undo more: rewind undo")));
    Ok(())
}

fn cmd_goto(r: &str) -> Result<()> {
    let repo = open(false)?;
    let _lock = repo.lock()?;
    let target = resolve(&repo.load()?, r)?;
    snapshot_locked(&repo, "save", "unsaved changes (before goto)", None, None, None, false)?;
    let snaps = repo.load()?;
    let cur = state_end(&snaps);
    let want = state_at(&snaps, target);
    if cur == want {
        println!("already at the state of #{target}");
        return Ok(());
    }
    let from = snaps.last().unwrap().id;
    let ch = apply_state(&repo, &cur, &want, find(&snaps, target).unwrap().time)?;
    snapshot_locked(&repo, "goto", &format!("back to #{target}"), None, Some(target), None, true)?;
    println!("{} project now matches #{target}", green("rewind:"));
    print!("{}", change_lines(&repo, &Side::Snap(&cur), &Side::Snap(&want), &ch));
    println!("{}", dim(&format!("  return: rewind goto {from}")));
    Ok(())
}

fn cmd_restore(paths: Vec<String>, at: Option<String>) -> Result<()> {
    let repo = open(false)?;
    let rels: Vec<String> = paths.iter().map(|p| rel_path(&repo, p)).collect::<Result<_>>()?;
    let _lock = repo.lock()?;
    let snaps0 = repo.load()?;
    let last0 = snaps0.last().ok_or_else(|| anyhow!("no snapshots yet"))?.id;
    let cur0 = state_end(&snaps0);
    let work = working_state(&repo)?;

    let sub = |st: &State, p: &str| -> Vec<(String, Entry)> {
        st.iter().filter(|(k, _)| under(k, p)).map(|(k, e)| (k.clone(), e.clone())).collect()
    };
    let mut targets = Vec::new();
    for p in &rels {
        let t = match &at {
            Some(a) => resolve(&snaps0, a)?,
            // Unsaved edits? Throw those away. Otherwise go to before the last recorded change.
            None if sub(&cur0, p) != sub(&work, p) => last0,
            None => {
                let s = snaps0
                    .iter()
                    .rev()
                    .find(|s| s.changes.iter().any(|c| under(&c.path, p)))
                    .ok_or_else(|| anyhow!("no history for {p}"))?;
                prev_id(&snaps0, s.id)
                    .ok_or_else(|| anyhow!("{p} hasn't changed since tracking began (#{})", s.id))?
            }
        };
        targets.push((p.clone(), t));
    }

    snapshot_locked(&repo, "save", "unsaved changes (before restore)", None, None, None, false)?;
    let snaps = repo.load()?;
    let cur = state_end(&snaps);
    let mut want = cur.clone();
    for (p, t) in &targets {
        let ts = state_at(&snaps, *t);
        want.retain(|k, _| !under(k, p));
        want.extend(sub(&ts, p));
    }
    if want == cur {
        println!("already matches; pick an older version from `rewind log {}`", paths.join(" "));
        return Ok(());
    }
    let since = targets.iter().filter_map(|(_, t)| find(&snaps, *t)).map(|s| s.time).min().unwrap_or(0);
    let ch = apply_state(&repo, &cur, &want, since)?;
    let desc: Vec<String> = targets.iter().map(|(p, t)| format!("{} ← #{t}", if p.is_empty() { "." } else { p })).collect();
    let (s, _) = snapshot_locked(&repo, "restore", &desc.join(", "), None, targets.first().map(|t| t.1), None, true)?;
    println!("{} restored {}", green("rewind:"), desc.join(", "));
    print!("{}", change_lines(&repo, &Side::Snap(&cur), &Side::Snap(&want), &ch));
    if let Some(s) = s {
        println!("{}", dim(&format!("  oops? rewind undo  (undoes #{})", s.id)));
    }
    Ok(())
}

fn cmd_cat(path: &str, at: Option<String>) -> Result<()> {
    let repo = open(false)?;
    let snaps = repo.load()?;
    let id = match at {
        Some(a) => resolve(&snaps, &a)?,
        None => snaps.last().ok_or_else(|| anyhow!("no snapshots yet"))?.id,
    };
    let rel = rel_path(&repo, path)?;
    let st = state_at(&snaps, id);
    let e = st.get(&rel).ok_or_else(|| anyhow!("{rel} did not exist in #{id}"))?;
    std::io::stdout().write_all(&repo.get(&e.hash)?)?;
    Ok(())
}

fn cmd_prune(older: &str) -> Result<()> {
    let secs = parse_dur(older).ok_or_else(|| anyhow!("bad duration `{older}` (try 7d, 12h, 30m)"))?;
    let repo = open(false)?;
    let _lock = repo.lock()?;
    let snaps = repo.load()?;
    let cutoff = now() - secs;
    let folded = snaps.iter().rposition(|s| s.time < cutoff).unwrap_or(0);
    let kept = if folded > 0 {
        let base = &snaps[folded];
        let st = state_at(&snaps, base.id);
        let mut out = vec![Snapshot {
            id: base.id,
            time: base.time,
            kind: "base".into(),
            label: format!("{folded} older snapshots folded in"),
            group: None,
            restores: None,
            changes: st
                .iter()
                .map(|(p, e)| Change { op: 'A', path: p.clone(), hash: Some(e.hash.clone()), mode: e.mode })
                .collect(),
        }];
        for s in &snaps[folded + 1..] {
            let mut s = s.clone();
            s.group = s.group.map(|g| g.max(base.id));
            s.restores = s.restores.map(|r| r.max(base.id));
            out.push(s);
        }
        repo.rewrite(&out)?;
        out
    } else {
        snaps
    };

    // Delete objects no snapshot points to. Skip fresh ones: a running watcher may be about to use them.
    let live: HashSet<&str> = kept.iter().flat_map(|s| s.changes.iter().filter_map(|c| c.hash.as_deref())).collect();
    let fresh = SystemTime::now() - Duration::from_secs(600);
    let (mut n, mut bytes) = (0u64, 0u64);
    for sub in fs::read_dir(repo.objects())?.flatten() {
        let prefix = sub.file_name().to_string_lossy().into_owned();
        for f in fs::read_dir(sub.path())?.flatten() {
            let name = format!("{prefix}{}", f.file_name().to_string_lossy());
            let Ok(md) = f.metadata() else { continue };
            if !live.contains(name.as_str()) && md.modified().is_ok_and(|m| m < fresh) && fs::remove_file(f.path()).is_ok() {
                n += 1;
                bytes += md.len();
            }
        }
    }
    repo.drop_index();
    println!(
        "{} folded {folded} snapshots older than {older}, removed {n} stored versions, freed {}",
        green("rewind:"),
        human(bytes)
    );
    Ok(())
}
