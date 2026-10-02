//! Path, process, ignore-pattern and filesystem helpers.

use crate::dbg_log;
use crate::error::{Error, Result};
use crate::log;
use regex::Regex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ENV_TEMP: &str = "DOTDROP_TMPDIR";

/// the temporary directory (created lazily)
static TMPDIR: OnceLock<String> = OnceLock::new();

pub fn header() -> &'static str {
    "This dotfile is managed using dotdrop"
}

////////////////////////////////////////////////////////////
// python-like path helpers (string based for fidelity)
////////////////////////////////////////////////////////////

pub fn is_abs(p: &str) -> bool {
    p.starts_with('/')
}

/// os.path.join for two components
pub fn join(a: &str, b: &str) -> String {
    if is_abs(b) || a.is_empty() {
        return b.to_string();
    }
    if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// os.path.dirname
pub fn dirname(p: &str) -> String {
    match p.rfind('/') {
        None => String::new(),
        Some(i) => {
            let head = &p[..=i];
            if head.chars().all(|c| c == '/') {
                head.to_string()
            } else {
                head.trim_end_matches('/').to_string()
            }
        }
    }
}

/// os.path.basename
pub fn basename(p: &str) -> String {
    match p.rfind('/') {
        None => p.to_string(),
        Some(i) => p[i + 1..].to_string(),
    }
}

/// os.path.split
pub fn split(p: &str) -> (String, String) {
    (dirname(p), basename(p))
}

/// os.path.normpath
pub fn normpath(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let initial = if p.starts_with("//") && !p.starts_with("///") {
        2
    } else if p.starts_with('/') {
        1
    } else {
        0
    };
    let mut comps: Vec<&str> = Vec::new();
    for c in p.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c != ".."
            || (initial == 0 && comps.is_empty())
            || comps.last().is_some_and(|l| *l == "..")
        {
            comps.push(c);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let joined = comps.join("/");
    let res = format!("{}{}", "/".repeat(initial), joined);
    if res.is_empty() { ".".to_string() } else { res }
}

pub fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}

/// os.path.abspath
pub fn abspath(p: &str) -> String {
    if is_abs(p) {
        normpath(p)
    } else {
        normpath(&join(&cwd(), p))
    }
}

/// os.path.expanduser
pub fn expanduser(p: &str) -> String {
    if !p.starts_with('~') {
        return p.to_string();
    }
    let end = p.find('/').unwrap_or(p.len());
    let user = &p[1..end];
    let home = if user.is_empty() {
        std::env::var("HOME").ok().or_else(|| {
            nix::unistd::User::from_uid(nix::unistd::getuid())
                .ok()
                .flatten()
                .map(|u| u.dir.to_string_lossy().into_owned())
        })
    } else {
        nix::unistd::User::from_name(user)
            .ok()
            .flatten()
            .map(|u| u.dir.to_string_lossy().into_owned())
    };
    match home {
        None => p.to_string(),
        Some(h) => {
            let h = if h.len() > 1 {
                h.trim_end_matches('/').to_string()
            } else {
                h
            };
            format!("{}{}", h, &p[end..])
        }
    }
}

pub fn home() -> String {
    expanduser("~")
}

/// os.path.expandvars
pub fn expandvars(p: &str) -> String {
    if !p.contains('$') {
        return p.to_string();
    }
    let chars: Vec<char> = p.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let (name, next) = if i + 1 < chars.len() && chars[i + 1] == '{' {
            match chars[i + 2..].iter().position(|c| *c == '}') {
                Some(e) => (
                    chars[i + 2..i + 2 + e].iter().collect::<String>(),
                    i + 3 + e,
                ),
                None => ("".to_string(), i),
            }
        } else {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            (chars[i + 1..j].iter().collect::<String>(), j)
        };
        if name.is_empty() {
            out.push('$');
            i += 1;
            continue;
        }
        match std::env::var(&name) {
            Ok(v) => out.push_str(&v),
            Err(_) => out.extend(&chars[i..next]),
        }
        i = next;
    }
    out
}

/// os.path.relpath
pub fn relpath(path: &str, start: &str) -> String {
    let p = abspath(path);
    let s = abspath(start);
    let pc: Vec<&str> = p.split('/').filter(|x| !x.is_empty()).collect();
    let sc: Vec<&str> = s.split('/').filter(|x| !x.is_empty()).collect();
    let common = pc.iter().zip(sc.iter()).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; sc.len() - common];
    parts.extend(&pc[common..]);
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

pub fn lexists(p: &str) -> bool {
    fs::symlink_metadata(p).is_ok()
}

pub fn exists(p: &str) -> bool {
    fs::metadata(p).is_ok()
}

pub fn is_dir(p: &str) -> bool {
    fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

pub fn is_file(p: &str) -> bool {
    fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

pub fn is_link(p: &str) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// os.path.realpath
pub fn realpath(p: &str) -> String {
    match fs::canonicalize(p) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => {
            // dangling: resolve parent and keep the leaf (best effort)
            let abs = abspath(p);
            let parent = dirname(&abs);
            match fs::canonicalize(&parent) {
                Ok(rp) => join(&rp.to_string_lossy(), &basename(&abs)),
                Err(_) => abs,
            }
        }
    }
}

/// os.listdir (unordered like the original; names only)
pub fn listdir(p: &str) -> Vec<String> {
    match fs::read_dir(p) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// os.walk(top, followlinks) -> (root, dirs, files)
pub fn walk(top: &str, followlinks: bool) -> Vec<(String, Vec<String>, Vec<String>)> {
    let mut res = Vec::new();
    let mut stack = vec![top.to_string()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_dir(&join(&dir, &name)) {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
        for d in dirs.iter().rev() {
            let full = join(&dir, d);
            if followlinks || !is_link(&full) {
                stack.push(full);
            }
        }
        res.push((dir, dirs, files));
    }
    res
}

pub fn dir_empty(p: &str) -> bool {
    if !is_dir(p) {
        return true;
    }
    listdir(p).is_empty()
}

pub fn strip_home(path: &str) -> String {
    let h = format!("{}/", home());
    match path.strip_prefix(&h) {
        Some(rest) => rest.to_string(),
        None => path.to_string(),
    }
}

/// change path to be under newdir
pub fn pivot_path(path: &str, newdir: &str, striphome: bool) -> String {
    let path = if striphome {
        strip_home(path)
    } else {
        path.to_string()
    };
    let sub = path.trim_start_matches('/');
    let new = join(newdir, sub);
    dbg_log!("pivot \"{path}\" to \"{new}\"");
    new
}

pub fn uniq_list<T: PartialEq + Clone>(list: &[T]) -> Vec<T> {
    let mut new: Vec<T> = Vec::new();
    for e in list {
        if !new.contains(e) {
            new.push(e.clone());
        }
    }
    new
}

////////////////////////////////////////////////////////////
// processes
////////////////////////////////////////////////////////////

/// run a command, capture stdout and stderr merged; returns (exit-code, output)
fn run_merged(cmd: &mut Command) -> std::io::Result<(i32, Vec<u8>)> {
    let (mut reader, writer) = std::io::pipe()?;
    cmd.stdin(Stdio::null());
    cmd.stdout(writer.try_clone()?);
    cmd.stderr(writer);
    let mut child = cmd.spawn()?;
    // release our copies of the write end held by the Command
    *cmd = Command::new("true");
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    let status = child.wait()?;
    Ok((status.code().unwrap_or(-1), buf))
}

/// run a command (program + args, no shell); returns (success, output)
pub fn run(cmd: &[String]) -> (bool, String) {
    dbg_log!("exec: {}", cmd.join(" "));
    let Some((prog, args)) = cmd.split_first() else {
        return (false, String::new());
    };
    match run_merged(Command::new(prog).args(args)) {
        Ok((code, out)) => (code == 0, String::from_utf8_lossy(&out).into_owned()),
        Err(e) => (false, e.to_string()),
    }
}

/// run a command through the shell, like python's `getstatusoutput`
pub fn shellrun(cmd: &str) -> (bool, String) {
    dbg_log!("shell exec: \"{cmd}\"");
    let res = run_merged(Command::new("sh").arg("-c").arg(cmd));
    let (ret, out) = match res {
        Ok((code, out)) => {
            let mut s = String::from_utf8_lossy(&out).into_owned();
            if s.ends_with('\n') {
                s.pop();
            }
            (code, s)
        }
        Err(e) => (127, e.to_string()),
    };
    dbg_log!("shell result ({ret}): {out}");
    (ret == 0, out)
}

/// execute a user command with the user's shell, inheriting stdio
pub fn shell_call(cmd: &str) -> i32 {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    match Command::new(shell).arg("-c").arg(cmd).status() {
        Ok(st) => st.code().unwrap_or(-1),
        Err(e) => {
            log::err(format!("unable to execute shell: {e}"));
            127
        }
    }
}

pub fn userinput(prompt: &str) -> String {
    dbg_log!("get user input for \"{prompt}\"");
    let res = log::input(&format!("Please provide the value for \"{prompt}\": "));
    dbg_log!("user input result: {res}");
    res
}

/// compare two files, returns '' if same
pub fn diff(original: &str, modified: &str, diff_cmd: &str) -> String {
    let cmdline = if diff_cmd.is_empty() {
        "diff -r -u {0} {1}"
    } else {
        diff_cmd
    };
    let cmd: Vec<String> = cmdline
        .split_whitespace()
        .map(|x| match x {
            "{0}" | "{original}" => original.to_string(),
            "{1}" | "{modified}" => modified.to_string(),
            _ => x.to_string(),
        })
        .collect();
    run(&cmd).1
}

/// path of an executable in PATH (or the path itself when it contains a /)
pub fn which(name: &str, path: Option<&str>) -> Option<String> {
    let executable = |p: &str| {
        fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if name.contains('/') {
        return executable(name).then(|| name.to_string());
    }
    let pathvar = match path {
        Some(p) => p.to_string(),
        None => std::env::var("PATH").unwrap_or_default(),
    };
    pathvar
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| join(d, name))
        .find(|c| executable(c))
}

pub fn is_bin_in_path(command: &str) -> bool {
    let Some(binary) = command.split(' ').next() else {
        return false;
    };
    !binary.is_empty() && which(binary, None).is_some()
}

////////////////////////////////////////////////////////////
// temporary files
////////////////////////////////////////////////////////////

fn make_tmpdir() -> String {
    if let Ok(t) = std::env::var(ENV_TEMP) {
        let t = normpath(&abspath(&expanduser(&t)));
        if fs::create_dir_all(&t).is_ok() {
            return t;
        }
    }
    tempfile::Builder::new()
        .prefix("dotdrop-")
        .tempdir()
        .map(|d| d.keep().to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/tmp".to_string())
}

pub fn get_tmpdir() -> String {
    TMPDIR.get_or_init(make_tmpdir).clone()
}

pub fn write_to_tmpfile(content: &[u8]) -> Result<String> {
    let f = tempfile::Builder::new()
        .prefix("dotdrop-")
        .tempfile_in(get_tmpdir())?;
    let (_, path) = f.keep().map_err(|e| Error::Io(e.error))?;
    fs::write(&path, content)?;
    Ok(path.to_string_lossy().into_owned())
}

pub fn get_unique_tmp_name() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    join(
        &get_tmpdir(),
        &format!("{:x}-{:x}", t, std::process::id() as u64 ^ rand_u64()),
    )
}

fn rand_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CNT: AtomicU64 = AtomicU64::new(0);
    CNT.fetch_add(1, Ordering::Relaxed)
}

////////////////////////////////////////////////////////////
// filesystem
////////////////////////////////////////////////////////////

fn noremove() -> Vec<String> {
    vec![normpath(&home()), normpath(&join(&home(), ".config"))]
}

/// remove a file/directory/symlink; returns false (with warning) on failure
pub fn removepath(path: &str) -> bool {
    if path.is_empty() || !lexists(path) {
        return true;
    }
    if noremove().contains(&normpath(&expanduser(path))) {
        log::warn(format!("Dotdrop refuses to remove {path}"));
        return false;
    }
    dbg_log!("removing {path}");
    match try_removepath(path) {
        Ok(()) => true,
        Err(e) => {
            log::warn(e);
            false
        }
    }
}

/// like removepath but reports the error to the caller
pub fn try_removepath(path: &str) -> std::result::Result<(), String> {
    if path.is_empty() || !lexists(path) {
        return Ok(());
    }
    if noremove().contains(&normpath(&expanduser(path))) {
        let err = format!("Dotdrop refuses to remove {path}");
        log::err(&err);
        return Err(err);
    }
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let res = if meta.file_type().is_symlink() || meta.is_file() {
        fs::remove_file(path)
    } else if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        return Err(format!("Unsupported file type for deletion: {path}"));
    };
    res.map_err(|e| e.to_string())
}

pub fn samefile(a: &str, b: &str) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// True when both files have identical content
pub fn same_content(a: &str, b: &str) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    let (mut ba, mut bb) = (vec![0u8; 65536], vec![0u8; 65536]);
    loop {
        let na = read_full(&mut fa, &mut ba);
        let nb = read_full(&mut fb, &mut bb);
        match (na, nb) {
            (Ok(x), Ok(y)) if x == y => {
                if x == 0 {
                    return true;
                }
                if ba[..x] != bb[..y] {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let r = f.read(&mut buf[n..])?;
        if r == 0 {
            break;
        }
        n += r;
    }
    Ok(n)
}

/// shutil.copy2: copy content, mode and mtime (follows symlinks)
pub fn copy2(src: &str, dst: &str) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    if let Ok(m) = fs::metadata(src) {
        if m.file_type().is_fifo() {
            return Err(std::io::Error::other(format!("`{src}` is a named pipe")));
        }
    }
    let target = if is_dir(dst) {
        join(dst, &basename(src))
    } else {
        dst.to_string()
    };
    fs::copy(src, &target)?;
    let meta = fs::metadata(src)?;
    if let Ok(mtime) = meta.modified() {
        let f = fs::OpenOptions::new().write(true).open(&target)?;
        let _ = f.set_modified(mtime);
    }
    Ok(())
}

/// shutil.copytree (dst must not exist)
pub fn copytree(src: &str, dst: &str) -> std::io::Result<()> {
    let meta = fs::metadata(src)?;
    fs::create_dir(dst)?;
    for name in listdir(src) {
        let s = join(src, &name);
        let d = join(dst, &name);
        if is_dir(&s) {
            copytree(&s, &d)?;
        } else {
            copy2(&s, &d)?;
        }
    }
    fs::set_permissions(dst, meta.permissions())?;
    Ok(())
}

/// copy a regular file creating the destination directory; true if copied
pub fn copyfile(src: &str, dst: &str) -> bool {
    if !is_file(src) {
        dbg_log!("ignore special file \"{src}\"");
        return false;
    }
    let dstdir = dirname(dst);
    dbg_log!("mkdir \"{dstdir}\"");
    if !dstdir.is_empty() && fs::create_dir_all(&dstdir).is_err() {
        return false;
    }
    dbg_log!("cp {src} {dst}");
    copy2(src, dst).is_ok() && exists(dst)
}

pub fn content_empty(content: &[u8]) -> bool {
    content.is_empty() || content == b"\n"
}

pub fn get_file_perm(path: &str) -> u32 {
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o777)
}

pub fn chmod(path: &str, mode: u32) -> bool {
    dbg_log!("chmod {mode:o} {path}");
    if fs::set_permissions(path, fs::Permissions::from_mode(mode)).is_err() {
        return false;
    }
    get_file_perm(path) == mode
}

pub fn mirror_file_rights(src: &str, dst: &str) -> std::io::Result<()> {
    if !exists(src) || !exists(dst) {
        return Ok(());
    }
    fs::set_permissions(dst, fs::Permissions::from_mode(get_file_perm(src)))
}

pub fn get_umask() -> u32 {
    use nix::sys::stat::{Mode, umask};
    let cur = umask(Mode::empty());
    umask(cur);
    cur.bits() as u32
}

pub fn get_default_file_perms(path: &str, umask: u32) -> u32 {
    let base = if is_dir(path) { 0o777 } else { 0o666 };
    base - umask
}

////////////////////////////////////////////////////////////
// ignore patterns (python fnmatch semantics)
////////////////////////////////////////////////////////////

/// translate a shell pattern to an anchored regex (like fnmatch.translate)
fn fnmatch_translate(pat: &str) -> String {
    let p: Vec<char> = pat.chars().collect();
    let n = p.len();
    let mut i = 0;
    let mut res = String::from("(?s)^");
    while i < n {
        let c = p[i];
        i += 1;
        match c {
            '*' => {
                while i < n && p[i] == '*' {
                    i += 1;
                }
                res.push_str(".*");
            }
            '?' => res.push('.'),
            '[' => {
                let mut j = i;
                if j < n && p[j] == '!' {
                    j += 1;
                }
                if j < n && p[j] == ']' {
                    j += 1;
                }
                while j < n && p[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    res.push_str("\\[");
                } else {
                    let stuff: String = p[i..j].iter().collect();
                    let mut stuff = stuff.replace('\\', "\\\\");
                    stuff = stuff
                        .replace('[', "\\[")
                        .replace('&', "\\&")
                        .replace('~', "\\~");
                    i = j + 1;
                    if let Some(rest) = stuff.strip_prefix('!') {
                        stuff = format!("^{rest}");
                    } else if stuff.starts_with('^') {
                        stuff = format!("\\{stuff}");
                    }
                    res.push('[');
                    res.push_str(&stuff);
                    res.push(']');
                }
            }
            _ => res.push_str(&regex::escape(&c.to_string())),
        }
    }
    res.push('$');
    res
}

thread_local! {
    static FNCACHE: RefCell<HashMap<String, Option<Regex>>> = RefCell::new(HashMap::new());
}

/// fnmatch.fnmatch (posix: case sensitive)
pub fn fnmatch(name: &str, pat: &str) -> bool {
    FNCACHE.with(|c| {
        let mut c = c.borrow_mut();
        let re = c
            .entry(pat.to_string())
            .or_insert_with(|| Regex::new(&fnmatch_translate(pat)).ok());
        re.as_ref().is_some_and(|r| r.is_match(name))
    })
}

/// path or any of its parents matches the pattern
fn match_ignore_pattern(path: &str, pattern: &str) -> bool {
    let mut subpath = path.to_string();
    while subpath != "/" && !subpath.is_empty() {
        if fnmatch(&subpath, pattern) {
            dbg_log!("ignore \"{pattern}\" match: {subpath} ({path})");
            return true;
        }
        let next = dirname(&subpath);
        if next == subpath {
            break;
        }
        subpath = next;
    }
    false
}

fn must_ignore_one(path: &str, ignores: &[&String], neg_ignores: &[&String], strict: bool) -> bool {
    let mut matched = false;
    for pattern in ignores {
        if match_ignore_pattern(path, pattern) {
            matched = true;
        }
    }
    let mut neg_cnt = 0;
    for pattern in neg_ignores {
        let pattern = &pattern[1..];
        neg_cnt += 1;
        if !match_ignore_pattern(path, pattern) {
            dbg_log!("NO MATCH negative ignore \"{pattern}\" against {path}");
            continue;
        }
        dbg_log!("MATCH negative ignore \"{pattern}\" against {path}");
        if matched {
            matched = false;
        } else {
            log::warn(format!(
                "no files that are currently being ignored match \"{pattern}\". In order for a \
                 negative ignore pattern to work, it must match a file that is being ignored by a \
                 previous ignore pattern."
            ));
        }
    }
    if !matched {
        return false;
    }
    if !strict && (is_dir(path) || !exists(path)) && neg_cnt > 0 {
        dbg_log!(
            "[!!] ignore would have match but neg ignores present and is a dir or does not exist: \"{path}\" -> not ignored!"
        );
        return false;
    }
    true
}

/// true if any path matches any ignore pattern
pub fn must_ignore(paths: &[&str], ignores: &[String], strict: bool) -> bool {
    if ignores.is_empty() {
        return false;
    }
    dbg_log!("[IGN] IGNORE? \"{paths:?}\" against {ignores:?}");
    let (neg, pos): (Vec<&String>, Vec<&String>) = ignores.iter().partition(|i| i.starts_with('!'));
    for path in paths {
        if must_ignore_one(path, &pos, &neg, strict) {
            dbg_log!("[IGN] IGNORING \"{paths:?}\"");
            return true;
        }
    }
    dbg_log!("[IGN] NOT IGNORING \"{paths:?}\"");
    false
}

/// allow relative ignore patterns
pub fn ignores_to_absolute(ignores: &[String], prefixes: &[&str]) -> Vec<String> {
    let mut new = Vec::new();
    dbg_log!("ignores before patching: {ignores:?}");
    for ignore in ignores {
        let (neg, pat) = match ignore.strip_prefix('!') {
            Some(r) => ("!", r),
            None => ("", ignore.as_str()),
        };
        if is_abs(pat) || (pat.contains('*') && (pat.starts_with('*') || pat.starts_with('/'))) {
            new.push(format!("{neg}{pat}"));
            continue;
        }
        for prefix in prefixes {
            new.push(format!("{neg}{}", join(prefix, pat)));
        }
    }
    dbg_log!("ignores after patching: {new:?}");
    new
}

/// list owned by os-release style file
pub fn os_release_field(field: &str) -> String {
    for file in ["/etc/os-release", "/usr/lib/os-release"] {
        if let Ok(content) = fs::read_to_string(file) {
            for line in content.lines() {
                if let Some(v) = line.strip_prefix(&format!("{field}=")) {
                    return v.trim().trim_matches('"').trim_matches('\'').to_string();
                }
            }
        }
    }
    String::new()
}

/// OsStr to String helper
pub fn os(s: &OsStr) -> String {
    s.to_string_lossy().into_owned()
}

/// compare version strings made of dot-separated integers
pub fn parse_version(v: &str) -> Option<Vec<u64>> {
    v.split('.').map(|x| x.parse::<u64>().ok()).collect()
}

/// warn when a newer release exists on github
pub fn check_version() {
    let url = "https://api.github.com/repos/deadc0de6/dotdrop/releases/latest";
    let (ok, out) = run(&[
        "curl".into(),
        "-s".into(),
        "--max-time".into(),
        "1".into(),
        url.into(),
    ]);
    if !ok {
        return;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&out) else {
        return;
    };
    let Some(name) = json.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    let latest = name.strip_prefix('v').unwrap_or(name);
    if let (Some(cur), Some(lat)) = (parse_version(VERSION), parse_version(latest)) {
        if cur < lat {
            log::warn(format!("A new version of dotdrop is available ({latest})"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_patterns() {
        assert_eq!(normpath("/a/./b//c/../d"), "/a/b/d");
        assert_eq!(dirname("/a/b/"), "/a/b");
        assert_eq!(relpath("/a/b/c", "/a/d"), "../b/c");
        assert!(fnmatch("/x/y/file.bak", "*.bak"));
        assert!(fnmatch("a1", "a[!2]"));
        assert!(!fnmatch("a2", "a[!2]"));
        // negative patterns re-include what a previous pattern ignored
        let ign = vec!["*/dir/*".to_string(), "!*/keep".to_string()];
        assert!(must_ignore(&["/t/dir/x"], &ign, true));
        assert!(!must_ignore(&["/t/dir/keep"], &ign, true));
    }
}
