//! Join an existing network namespace and exec a program (like `ip netns exec`, minimal).

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use runns::{is_named_netns_path, owner_stamp_path, parse_owner_stamp};

const CLONE_NEWNET: i32 = 0x4000_0000;
const CLONE_NEWNS: i32 = 0x0002_0000;
const O_RDONLY: i32 = 0;
const MS_BIND: libc::c_ulong = 4096;
const MS_REC: libc::c_ulong = 16384;
const MS_SLAVE: libc::c_ulong = 1 << 19;
const CAP_SYS_ADMIN: u32 = 21;
const CAP_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn usage() -> &'static str {
    "Usage: runns <netns> [--] <command> [args...]\n\
     \n\
     Run COMMAND in Linux network namespace NETNS.\n\
     NETNS is a name under /run/netns (or /var/run/netns), or an absolute path\n\
     to a netns inode (e.g. /var/run/netns/myvpn).\n\
     \n\
     Environment:\n\
       RUNNS_NETNS   default namespace if omitted as first argument (optional)\n\
     \n\
     Example:\n\
       sudo runns agent -- curl -4 https://ifconfig.me\n\
     \n\
     sudo runns joins the namespace as root, then runs COMMAND as the user\n\
     who invoked sudo (SUDO_UID). Host proxy variables are removed: they point\n\
     at 127.0.0.1 on the host, which is not this namespace's loopback.\n"
}

fn netns_etc_name(user_arg: &str, resolved_path: &Path) -> String {
    if user_arg.starts_with('/') {
        resolved_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(user_arg)
            .to_string()
    } else {
        user_arg.to_string()
    }
}

fn netns_path(name: &str) -> PathBuf {
    if name.starts_with('/') {
        return PathBuf::from(name);
    }
    let run = Path::new("/run/netns").join(name);
    if run.exists() {
        run
    } else {
        Path::new("/var/run/netns").join(name)
    }
}

enum ParseOutcome {
    Run(PathBuf, String, Vec<String>),
    Help,
    Error(i32),
}

fn parse_args(args: impl Iterator<Item = String>) -> ParseOutcome {
    let mut args = args.peekable();

    if args.peek().is_some_and(|a| a == "-h" || a == "--help") {
        print!("{}", usage());
        return ParseOutcome::Help;
    }

    let netns_name = match args.next() {
        None => match std::env::var("RUNNS_NETNS") {
            Ok(n) if !n.is_empty() => n,
            _ => {
                eprint!("runns: missing network namespace name\n\n{}", usage());
                return ParseOutcome::Error(2);
            }
        },
        Some(n) if n == "--" => {
            eprint!("runns: missing network namespace name\n\n{}", usage());
            return ParseOutcome::Error(2);
        }
        Some(n) => n,
    };

    if args.peek().is_some_and(|a| a == "--") {
        args.next();
    }

    let command: Vec<String> = args.collect();
    if command.is_empty() {
        eprint!("runns: missing command\n\n{}", usage());
        return ParseOutcome::Error(2);
    }

    let path = netns_path(&netns_name);
    if !path.exists() {
        eprint!(
            "runns: network namespace {:?} not found (looked for {})\n",
            netns_name,
            path.display()
        );
        return ParseOutcome::Error(1);
    }

    let etc_name = netns_etc_name(&netns_name, &path);
    ParseOutcome::Run(path, etc_name, command)
}

fn mount_bind(src: &Path, dst: &Path) -> io::Result<()> {
    let src_c = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "netns etc path contains NUL"))?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "etc path contains NUL"))?;
    if unsafe {
        libc::mount(
            src_c.as_ptr(),
            dst_c.as_ptr(),
            std::ptr::null(),
            MS_BIND,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Like `ip netns exec`: bind-mount `/etc/netns/NAME/*` over `/etc/*` (caller already in mount ns).
fn bind_netns_etc(netns_name: &str) -> io::Result<()> {
    let etc_netns = Path::new("/etc/netns").join(netns_name);
    if !etc_netns.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&etc_netns)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let src = etc_netns.join(&name);
        let dst = Path::new("/etc").join(&name);
        mount_bind(&src, &dst)?;
        if name == "resolv.conf" {
            for extra in [
                "/run/systemd/resolve/stub-resolv.conf",
                "/run/systemd/resolve/resolv.conf",
            ] {
                let extra = Path::new(extra);
                if extra.exists() {
                    mount_bind(&src, extra)?;
                }
            }
        }
    }
    Ok(())
}

/// Match `ip netns exec`: setns → unshare mount ns → rslave `/` → bind per-netns `/etc` files.
fn enter_netns_with_etc(netns_c: &CString, etc_name: &str) -> io::Result<()> {
    let fd = unsafe { libc::open(netns_c.as_ptr(), O_RDONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let setns_err = if unsafe { libc::setns(fd, CLONE_NEWNET) } != 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        libc::close(fd);
    }
    if let Some(e) = setns_err {
        return Err(e);
    }

    if unsafe { libc::unshare(CLONE_NEWNS) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let root = CString::new("/").unwrap();
    if unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            MS_SLAVE | MS_REC,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }

    bind_netns_etc(etc_name)
}

fn cap_data() -> io::Result<[CapData; 2]> {
    let mut hdr = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    let mut data = [
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    let rc = unsafe { libc::syscall(libc::SYS_capget, &mut hdr, data.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(data)
}

fn has_cap_sys_admin() -> bool {
    cap_data()
        .map(|data| data[0].effective & (1 << CAP_SYS_ADMIN) != 0)
        .unwrap_or(false)
}

/// Drop every capability and block setuid/file-cap exec. The command keeps the caller's uid.
fn drop_all_capabilities() -> io::Result<()> {
    let hdr = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    let data = [
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    let rc = unsafe { libc::syscall(libc::SYS_capset, &hdr, data.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_owner_uid(name: &str) -> io::Result<Option<u32>> {
    let path = owner_stamp_path(name);
    match fs::read_to_string(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
        Ok(text) => match parse_owner_stamp(&text) {
            Some(uid) => Ok(Some(uid)),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad owner stamp {}", path.display()),
            )),
        },
    }
}

/// `sudo runns` (euid 0, SUDO_UID set) drops to that user after the namespace is joined.
/// A root shell without sudo stays root. A normal user needs `CAP_SYS_ADMIN`.
fn prepare_credentials(netns: &Path, netns_name: &str) -> io::Result<bool> {
    if unsafe { libc::getuid() } == 0 {
        return Ok(false);
    }
    if !is_named_netns_path(netns_name, netns) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "as a normal user, runns only joins /run/netns/<name>",
        ));
    }
    let uid = unsafe { libc::getuid() };
    match read_owner_uid(netns_name)? {
        None => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "namespace {netns_name} has no NETNS_USER owner; set it and restart netns-singbox, or run runns as root"
            ),
        )),
        Some(owner) if owner != uid => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("namespace {netns_name} allows uid {owner} only"),
        )),
        Some(_) if !has_cap_sys_admin() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "run with sudo (sudo runns <netns> -- <cmd> runs the command as you), or setcap cap_sys_admin+ep on the runns binary",
        )),
        Some(_) => Ok(true),
    }
}

/// `sudo` replaces `PATH` with `secure_path`. Search the current `PATH`, then ancestor
/// environments (the shell that invoked sudo) so `~/.local/bin` still resolves.
fn lookup_in_path(program: &str, path_var: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn path_from_environ(bytes: &[u8]) -> Option<&str> {
    for entry in bytes.split(|b| *b == 0) {
        let Ok(entry) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(path) = entry.strip_prefix("PATH=") {
            return Some(path);
        }
    }
    None
}

fn ppid_of(stat: &str) -> Option<i32> {
    let rest = stat.rsplit_once(')')?.1.trim_start();
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

fn ancestor_path_lookup(program: &str) -> Option<PathBuf> {
    let mut pid = std::process::id() as i32;
    for _ in 0..16 {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        pid = ppid_of(&stat)?;
        if pid <= 1 {
            break;
        }
        let Ok(environ) = fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        let Some(path) = path_from_environ(&environ) else {
            continue;
        };
        if let Some(found) = lookup_in_path(program, path) {
            return Some(found);
        }
    }
    None
}

struct RunAs {
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
    name: String,
    home: String,
    shell: String,
}

fn parse_sudo_id(value: Option<&str>) -> Option<u32> {
    let uid = value?.trim().parse::<u32>().ok()?;
    (uid != 0).then_some(uid)
}

fn cstr_copy(ptr: *const libc::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn group_list(user: &std::ffi::CStr, gid: u32) -> io::Result<Vec<u32>> {
    let mut count: libc::c_int = 32;
    let mut groups = vec![0u32; count as usize];
    let mut rc = unsafe { libc::getgrouplist(user.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
    if rc < 0 {
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        groups.resize(count as usize, 0);
        rc = unsafe { libc::getgrouplist(user.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    groups.truncate(count.max(0) as usize);
    Ok(groups)
}

/// Account from `SUDO_UID` when this process is root. `None` when not a sudo invocation.
fn run_as_from_sudo() -> io::Result<Option<RunAs>> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(None);
    }
    let Some(uid) = parse_sudo_id(std::env::var("SUDO_UID").ok().as_deref()) else {
        return Ok(None);
    };
    let mut buf = vec![0u8; 16 * 1024];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    if result.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("SUDO_UID {uid} has no passwd entry"),
        ));
    }
    let name = cstr_copy(pwd.pw_name);
    let home = cstr_copy(pwd.pw_dir);
    let shell = cstr_copy(pwd.pw_shell);
    let gid = std::env::var("SUDO_GID")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(pwd.pw_gid);
    let c_name = CString::new(name.clone())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "passwd name contains NUL"))?;
    let groups = group_list(&c_name, gid)?;
    Ok(Some(RunAs {
        uid,
        gid,
        groups,
        name,
        home,
        shell,
    }))
}

fn assume_user(user: &RunAs) -> io::Result<()> {
    if unsafe { libc::setgroups(user.groups.len(), user.groups.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setgid(user.gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setuid(user.uid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    drop_all_capabilities()
}

/// Host proxy vars point at 127.0.0.1, which inside the netns is an empty loopback.
const PROXY_ENV: &[&str] = &[
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "ftp_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "FTP_PROXY",
    "socks_proxy",
    "SOCKS_PROXY",
    "socks5_proxy",
    "SOCKS5_PROXY",
];

fn hide_host_proxies(cmd: &mut Command) {
    for key in PROXY_ENV {
        cmd.env_remove(*key);
    }
}

fn resolve_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    if let Some(path) = std::env::var_os("PATH") {
        if let Some(found) = lookup_in_path(program, &path.to_string_lossy()) {
            return Some(found);
        }
    }
    ancestor_path_lookup(program)
}

fn run_in_netns(netns: &Path, netns_name: &str, command: &[String]) -> io::Result<i32> {
    let program = &command[0];
    let netns = netns.to_path_buf();
    let netns_c = CString::new(netns.to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "netns path contains NUL"))?;
    let etc_name = netns_name.to_string();

    let drop_privs = prepare_credentials(&netns, netns_name)?;
    let run_as = if drop_privs {
        None
    } else {
        run_as_from_sudo()?
    };
    let program_path = resolve_program(program).unwrap_or_else(|| PathBuf::from(program));
    let mut cmd = Command::new(&program_path);
    cmd.args(&command[1..]);
    hide_host_proxies(&mut cmd);
    if let Some(user) = &run_as {
        if !user.home.is_empty() {
            cmd.env("HOME", &user.home);
        }
        if !user.name.is_empty() {
            cmd.env("USER", &user.name);
            cmd.env("LOGNAME", &user.name);
        }
        if !user.shell.is_empty() {
            cmd.env("SHELL", &user.shell);
        }
    }

    unsafe {
        cmd.pre_exec(move || {
            enter_netns_with_etc(&netns_c, &etc_name)?;
            if drop_privs {
                drop_all_capabilities()?;
            } else if let Some(user) = run_as.as_ref() {
                assume_user(user)?;
            }
            Ok(())
        });
    }

    match cmd.status() {
        Ok(status) => {
            if let Some(code) = status.code() {
                Ok(code)
            } else if let Some(sig) = status.signal() {
                Ok(128 + sig)
            } else {
                Ok(1)
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("runns: command not found: {program}");
            if std::env::var_os("SUDO_UID").is_some() && !program.contains('/') {
                eprintln!(
                    "runns: not on PATH or in the shell that invoked sudo; pass an absolute path"
                );
            }
            Ok(127)
        }
        Err(e) => Err(e),
    }
}

fn main() {
    let (netns, netns_name, command) = match parse_args(std::env::args().skip(1)) {
        ParseOutcome::Run(p, n, c) => (p, n, c),
        ParseOutcome::Help => std::process::exit(0),
        ParseOutcome::Error(code) => std::process::exit(code),
    };

    match run_in_netns(&netns, &netns_name, &command) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("runns: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netns_path_relative() {
        assert_eq!(netns_path("foo"), PathBuf::from("/var/run/netns/foo"));
    }

    #[test]
    fn netns_path_absolute() {
        assert_eq!(netns_path("/custom/ns"), PathBuf::from("/custom/ns"));
    }

    #[test]
    fn ppid_of_comm_with_paren() {
        assert_eq!(ppid_of("12 (ba)sh) S 34 12 12"), Some(34));
    }

    #[test]
    fn path_from_environ_extracts() {
        let bytes = b"HOME=/home/user\0PATH=/usr/bin:/home/user/.local/bin\0USER=user\0";
        assert_eq!(
            path_from_environ(bytes),
            Some("/usr/bin:/home/user/.local/bin")
        );
    }

    #[test]
    fn sudo_id_ignores_root() {
        assert_eq!(parse_sudo_id(Some("1000")), Some(1000));
        assert_eq!(parse_sudo_id(Some("0")), None);
        assert_eq!(parse_sudo_id(Some("")), None);
        assert_eq!(parse_sudo_id(None), None);
    }

    #[test]
    fn proxy_env_includes_https_proxy() {
        assert!(PROXY_ENV.contains(&"https_proxy"));
        assert!(PROXY_ENV.contains(&"all_proxy"));
    }

    #[test]
    fn lookup_in_path_finds_file() {
        let dir = std::env::temp_dir().join(format!("runns-lookup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("agent");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let found = lookup_in_path("agent", &format!("/usr/bin:{}", dir.display()));
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(found, Some(bin));
    }
}
