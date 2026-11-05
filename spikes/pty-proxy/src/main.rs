//! A transparent PTY proxy for the #228 spike. Bytes pass both ways unchanged;
//! the output also feeds a terminal emulator, whose screen and an activity
//! summary are written beside the log for the driver to compare.
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    env, fs,
    io::{self, Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

static WINCH: AtomicBool = AtomicBool::new(false);
static TERM_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}
extern "C" fn on_term(signal: libc::c_int) {
    TERM_SIGNAL.store(signal, Ordering::Relaxed);
}

fn install(signal: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
}

/// Puts the outer terminal in raw mode, so Ctrl+C and every key reach the child's
/// terminal as bytes, and restores it on drop.
struct RawMode(Option<libc::termios>);
impl RawMode {
    fn enter() -> Self {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return Self(None);
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
            Self(Some(saved))
        }
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = self.0 {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved);
            }
        }
    }
}

fn outer_size() -> PtySize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws);
    }
    PtySize {
        rows: if ws.ws_row == 0 { 24 } else { ws.ws_row },
        cols: if ws.ws_col == 0 { 80 } else { ws.ws_col },
        pixel_width: ws.ws_xpixel,
        pixel_height: ws.ws_ypixel,
    }
}

/// The last window title set with OSC 0 or 2 in `data`.
fn last_title(data: &[u8]) -> Option<String> {
    let mut found = None;
    let mut index = 0;
    while index + 4 < data.len() {
        if data[index] == 0x1b
            && data[index + 1] == b']'
            && matches!(data[index + 2], b'0' | b'2')
            && data[index + 3] == b';'
        {
            let start = index + 4;
            let end = data[start..]
                .iter()
                .position(|byte| *byte == 0x07 || *byte == 0x1b)
                .map(|offset| start + offset);
            if let Some(end) = end {
                found = Some(String::from_utf8_lossy(&data[start..end]).into_owned());
                index = end;
            }
        }
        index += 1;
    }
    found
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let prefix = PathBuf::from(
        args.next()
            .ok_or("usage: pty-proxy-spike PREFIX -- COMMAND...")?,
    );
    let mut rest = args.collect::<Vec<_>>();
    if rest.first().map(String::as_str) == Some("--") {
        rest.remove(0);
    }
    let (command, argv) = rest.split_first().ok_or("missing command")?;
    let pair = native_pty_system().openpty(outer_size())?;
    let mut builder = CommandBuilder::new(command);
    builder.args(argv);
    builder.cwd(env::current_dir()?);
    for (key, value) in env::vars() {
        builder.env(key, value);
    }
    let mut child = pair.slave.spawn_command(builder)?;
    drop(pair.slave);
    let pid = child.process_id().map_or(-1, |pid| pid as libc::pid_t);
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    install(libc::SIGWINCH, on_winch);
    install(libc::SIGTERM, on_term);
    install(libc::SIGHUP, on_term);
    let raw = RawMode::enter();
    let outer_gone = Arc::new(AtomicBool::new(false));

    {
        let outer_gone = outer_gone.clone();
        let stdin_is_terminal = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(count) if count > 0 => {
                        if writer.write_all(&buf[..count]).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                    _ => {
                        if stdin_is_terminal {
                            outer_gone.store(true, Ordering::Relaxed);
                        }
                        break;
                    }
                }
            }
        });
    }

    let size = outer_size();
    let parser = Arc::new(Mutex::new(vt100::Parser::new(size.rows, size.cols, 0)));
    let title = Arc::new(Mutex::new(String::new()));
    let bytes = Arc::new(AtomicU64::new(0));
    let last_output = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    let done = Arc::new(AtomicBool::new(false));
    {
        let (parser, title, bytes, last_output, done, outer_gone) = (
            parser.clone(),
            title.clone(),
            bytes.clone(),
            last_output.clone(),
            done.clone(),
            outer_gone.clone(),
        );
        let mut raw_log = fs::File::create(prefix.with_extension("raw"))?;
        thread::spawn(move || {
            let mut stdout = io::stdout().lock();
            let mut buf = [0u8; 65536];
            while let Ok(count) = reader.read(&mut buf) {
                if count == 0 {
                    break;
                }
                // Keep draining after the outer terminal goes, or the child blocks
                // on a full PTY and never sees its hangup.
                if stdout
                    .write_all(&buf[..count])
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    outer_gone.store(true, Ordering::Relaxed);
                }
                let _ = raw_log.write_all(&buf[..count]);
                bytes.fetch_add(count as u64, Ordering::Relaxed);
                last_output.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                if let Some(found) = last_title(&buf[..count]) {
                    *title.lock().unwrap() = found;
                }
                parser.lock().unwrap().process(&buf[..count]);
            }
            done.store(true, Ordering::Relaxed);
        });
    }

    let screen_path = prefix.with_extension("screen");
    let state_path = prefix.with_extension("state");
    let write_state = || {
        let now = started.elapsed().as_millis() as u64;
        let contents = parser.lock().unwrap().screen().contents();
        let _ = fs::write(&screen_path, contents);
        let quiet = now.saturating_sub(last_output.load(Ordering::Relaxed));
        let state = format!(
            "{{\"now_ms\":{now},\"quiet_ms\":{quiet},\"bytes\":{},\"rows\":{},\"cols\":{},\"title\":{:?}}}\n",
            bytes.load(Ordering::Relaxed),
            outer_size().rows,
            outer_size().cols,
            title.lock().unwrap()
        );
        let _ = fs::write(&state_path, state);
    };
    let mut last_write = Instant::now() - Duration::from_secs(1);
    let mut hung_up = false;
    // The child's exit ends the run, not the reader: the outer terminal can go while
    // the child lives, and a grandchild can hold the PTY open after the child exits.
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if WINCH.swap(false, Ordering::Relaxed) {
            let size = outer_size();
            let _ = pair.master.resize(size);
            parser
                .lock()
                .unwrap()
                .screen_mut()
                .set_size(size.rows, size.cols);
        }
        let signal = TERM_SIGNAL.swap(0, Ordering::Relaxed);
        if signal != 0 && pid > 0 {
            unsafe {
                libc::kill(-pid, signal);
            }
        }
        if !hung_up && pid > 0 && outer_gone.load(Ordering::Relaxed) {
            hung_up = true;
            unsafe {
                libc::kill(-pid, libc::SIGHUP);
            }
        }
        if last_write.elapsed() >= Duration::from_millis(200) {
            last_write = Instant::now();
            write_state();
        }
        thread::sleep(Duration::from_millis(10));
    };
    let drain = Instant::now();
    while !done.load(Ordering::Relaxed) && drain.elapsed() < Duration::from_millis(300) {
        thread::sleep(Duration::from_millis(5));
    }
    write_state();
    drop(raw);
    std::process::exit(status.exit_code() as i32);
}
