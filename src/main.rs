// cargo-jobserver: hold the machine's cargo jobserver fifo and keep it topped up.
// Cargo.toml: libc = "0.2"
use std::{
    ffi::CString,
    fs, io,
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    process::{Command, ExitCode},
    thread,
    time::Duration,
};

const FIFO: &str = "/tmp/cargo_jobserver";
const INTERVAL: Duration = Duration::from_secs(600);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cargo jobserver: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let n = token_count();

    if let Err(e) = fs::remove_file(FIFO)
        && e.kind() != io::ErrorKind::NotFound
    {
        eprintln!("cargo jobserver: warning: could not remove stale {FIFO}: {e}");
    }
    let path = CString::new(FIFO).expect("FIFO path has no NUL bytes");
    if unsafe { libc::mkfifo(path.as_ptr(), 0o644) } != 0 {
        return Err(io::Error::last_os_error()).map_err(ctx(&format!("creating fifo {FIFO}")));
    }
    // Read-write, so the open never blocks and the pipe stays alive while this handle lives.
    let mut fifo = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(FIFO)
        .map_err(ctx(&format!("opening fifo {FIFO}")))?;
    // A separate read-only, non-blocking handle for counting and draining tokens.
    // On macOS FIONREAD always reports 0 on a fifo opened read-write, so the
    // read-write handle above cannot be used to count.
    let mut counter = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(FIFO)
        .map_err(ctx(&format!("opening fifo {FIFO} for counting")))?;
    fifo.write_all(&vec![b'+'; n]).map_err(ctx("writing initial tokens"))?;

    let auth = format!("--jobserver-auth=fifo:{FIFO}");
    match Command::new("launchctl").args(["setenv", "MAKEFLAGS", &auth]).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "cargo jobserver: warning: `launchctl setenv MAKEFLAGS` exited with {s}; \
             new terminals will not see the jobserver"
        ),
        Err(e) => eprintln!(
            "cargo jobserver: warning: could not run launchctl: {e}; \
             new terminals will not see the jobserver"
        ),
    }
    println!("cargo jobserver: {n} tokens at {FIFO}");

    loop {
        thread::sleep(INTERVAL);
        if let Err(e) = reconcile(&mut fifo, &mut counter, n) {
            eprintln!(
                "cargo jobserver: error: {e}; will retry in {}s",
                INTERVAL.as_secs()
            );
        }
    }
}

/// The token count: CPUs minus 2, so the editor and viewer stay responsive during builds.
fn token_count() -> usize {
    let cpus = match thread::available_parallelism() {
        Ok(c) => c.get(),
        Err(e) => {
            eprintln!("cargo jobserver: warning: could not detect CPU count ({e}); assuming 4");
            4
        }
    };
    cpus.saturating_sub(2).max(1)
}

/// Bring the number of free tokens back to `n` while nothing is building.
///
/// FIONREAD only counts tokens sitting in the pipe, not ones a running build is
/// holding, so a count taken during a build is too low. We check for builds both
/// before and after counting and skip the round if either check sees one. A build
/// that starts and finishes entirely between the two checks can still slip through
/// and cause an over-fill, so surplus tokens are drained here too: any over-fill is
/// corrected on a later idle pass instead of persisting until restart.
fn reconcile(fifo: &mut fs::File, counter: &mut fs::File, n: usize) -> io::Result<()> {
    if building()? {
        return Ok(());
    }
    let free = free_tokens(counter)?;
    if free == n || building()? {
        return Ok(());
    }
    if free < n {
        let missing = n - free;
        fifo.write_all(&vec![b'+'; missing]).map_err(ctx("refilling tokens"))?;
        println!("cargo jobserver: {missing} token(s) were lost; refilled to {n}");
    } else {
        let surplus = free - n;
        let drained = drain(counter, surplus)?;
        println!("cargo jobserver: {surplus} surplus token(s) found; drained {drained} back toward {n}");
    }
    Ok(())
}

/// Number of tokens currently sitting in the fifo (FIONREAD: counts without consuming).
fn free_tokens(counter: &fs::File) -> io::Result<usize> {
    let mut len: libc::c_int = 0;
    if unsafe { libc::ioctl(counter.as_raw_fd(), libc::FIONREAD, &mut len) } != 0 {
        return Err(io::Error::last_os_error()).map_err(ctx("counting free tokens"));
    }
    Ok(usize::try_from(len).unwrap_or(0))
}

/// Remove up to `surplus` tokens without blocking. If a build grabbed some first,
/// fewer are removed and the rest is left for a later round.
fn drain(counter: &mut fs::File, surplus: usize) -> io::Result<usize> {
    let mut buf = vec![0u8; surplus];
    loop {
        match counter.read(&mut buf) {
            Ok(k) => return Ok(k),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(ctx("draining surplus tokens")(e)),
        }
    }
}

/// True if a cargo or rustc process is running. A failing check is an error rather
/// than "idle", so a broken pgrep can never trigger a refill in the middle of a build.
fn building() -> io::Result<bool> {
    let status = Command::new("pgrep")
        .args(["-qx", "cargo|rustc"])
        .status()
        .map_err(ctx("running pgrep"))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(io::Error::other(format!("pgrep failed with {status}"))),
    }
}

/// Prefix an io::Error with what we were doing when it happened.
fn ctx(what: &str) -> impl Fn(io::Error) -> io::Error + '_ {
    move |e| io::Error::new(e.kind(), format!("{what}: {e}"))
}
