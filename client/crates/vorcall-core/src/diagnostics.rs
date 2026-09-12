//! What the client leaves behind when something goes wrong: a size-rolling
//! debug log next to `config.toml`, the last lines of it kept in memory, and a
//! crash report written from the panic hook.
//!
//! Nothing here may take the app down with it. A directory that cannot be
//! created, a disk that is full, a poisoned lock: each is reported once on
//! stderr and then swallowed, because a client that cannot log is still a
//! client that works.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write as _};
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::config;
use crate::update::{self, Version};

/// The live log; the rotated generations are this name plus `.1`, `.2`, `.3`.
const LOG_FILE: &str = "vorcall.log";
/// A generation is closed and rotated once a write would take it past this.
const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// How many rotated generations survive behind the live file.
const KEEP: usize = 3;
/// How much of the log the panic hook can quote without reading it back.
const RECENT_LINES: usize = 200;
const KEEP_CRASHES: usize = 5;

const CRASH_PREFIX: &str = "crash-";
const CRASH_SUFFIX: &str = ".txt";

/// Overrides what reaches the file, the way `RUST_LOG` overrides the console.
const FILE_FILTER_ENV: &str = "RUST_LOG_FILE";
/// Debug from our own crates, info from everyone else's: the file exists to
/// answer "what was the client doing", which needs more than the console shows.
/// The GUI crate has no library target, so its tracing target is the bin name
/// `vorcall`, not the crate directory `vorcall_app`.
const FILE_FILTER: &str = "info,vorcall=debug,vorcall_core=debug,vorcall_voice=debug,vorcall_screen=debug,vorcall_hotkey=debug";

/// Set by [`init`] so the panic hook can quote the log without reading it back
/// — the disk may be exactly what is broken.
static RECENT: OnceLock<Recent> = OnceLock::new();

/// What [`init`] managed to set up. `error` is for the UI and the log itself,
/// never a reason to refuse to start.
pub struct LogSetup {
    pub file: Option<PathBuf>,
    pub error: Option<String>,
}

/// Installs the global subscriber: the console at `console_default` (or
/// `RUST_LOG`), and, when there is a config directory, the rolling file.
///
/// Callable once per process; a second call reports the refusal instead of
/// panicking.
pub fn init(console_default: &str) -> LogSetup {
    let console = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(console_default)),
        );

    let (rolling, error) = match config::log_dir() {
        None => (
            None,
            Some("this platform exposes no configuration directory".to_owned()),
        ),
        Some(dir) => match RollingFile::open(&dir) {
            Ok(rolling) => (Some(rolling), None),
            Err(e) => (
                None,
                Some(format!("cannot open {}: {e}", dir.join(LOG_FILE).display())),
            ),
        },
    };

    let file = rolling.as_ref().map(|rolling| rolling.path().to_path_buf());
    let file_layer = rolling.map(|rolling| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_timer(UtcTime)
            .with_writer(rolling)
            .with_filter(file_filter())
    });

    let recent = Recent::new();
    let result = tracing_subscriber::registry()
        .with(console)
        .with(file_layer)
        .with(recent.clone().with_filter(file_filter()))
        .try_init();

    match result {
        Ok(()) => {
            let _ = RECENT.set(recent);
            LogSetup { file, error }
        }
        Err(e) => LogSetup {
            file: None,
            error: Some(e.to_string()),
        },
    }
}

fn file_filter() -> EnvFilter {
    EnvFilter::try_from_env(FILE_FILTER_ENV).unwrap_or_else(|_| EnvFilter::new(FILE_FILTER))
}

/// `<config dir>/vorcall.log`, whether or not it exists yet.
pub fn log_path() -> Option<PathBuf> {
    config::log_dir().map(|dir| dir.join(LOG_FILE))
}

/// Every crash report on disk, oldest first — the name carries the time.
pub fn crash_reports() -> Vec<PathBuf> {
    match config::log_dir() {
        Some(dir) => reports_in(&dir),
        None => Vec::new(),
    }
}

pub fn remove_crash_reports() {
    for path in crash_reports() {
        let _ = fs::remove_file(path);
    }
}

/// Chains a crash report in front of the hook that is installed now, so a
/// release build leaves something behind and a debug build still prints.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_crash(info);
        previous(info);
    }));
}

/// Everything the hook does before handing back to the previous one. Nothing in
/// here may panic: a panic inside the panic hook aborts the process.
fn write_crash(info: &PanicHookInfo<'_>) {
    let crash = CrashInfo {
        version: Version::current().to_string(),
        platform: update::platform(),
        at: SystemTime::now(),
        thread: std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_owned(),
        message: panic_message(info),
        location: info.location().map(|location| location.to_string()),
        backtrace: std::backtrace::Backtrace::force_capture().to_string(),
    };
    let recent = RECENT.get().map(Recent::snapshot).unwrap_or_default();
    let report = crash_report_text(&crash, &recent);

    let Some(dir) = config::log_dir() else {
        return;
    };
    match write_crash_report(&dir, &report) {
        Ok(path) => eprintln!("vorcall: crash report written to {}", path.display()),
        Err(e) => eprintln!("vorcall: cannot write the crash report: {e}"),
    }
    prune_crash_reports(&dir, KEEP_CRASHES);
}

fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "<non-string payload>".to_owned()
    }
}

struct CrashInfo {
    version: String,
    platform: String,
    at: SystemTime,
    thread: String,
    message: String,
    location: Option<String>,
    backtrace: String,
}

fn crash_report_text(info: &CrashInfo, recent: &[String]) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "vorcall {} {}", info.version, info.platform);
    let _ = writeln!(text, "time: {}", rfc3339(info.at));
    let _ = writeln!(text, "thread: {}", info.thread);
    let _ = writeln!(text, "panic: {}", info.message);
    let _ = writeln!(
        text,
        "location: {}",
        info.location.as_deref().unwrap_or("<unknown>")
    );
    let _ = writeln!(text, "\nbacktrace:\n{}", info.backtrace.trim_end());
    let _ = writeln!(text, "\nlast log lines:");
    for line in recent {
        let _ = writeln!(text, "{line}");
    }
    text
}

/// Written through a temporary file like the session, so a second crash never
/// finds half a report, and never world-readable.
fn write_crash_report(dir: &Path, report: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;

    let name = format!("{CRASH_PREFIX}{}{CRASH_SUFFIX}", stamp(SystemTime::now()));
    let path = dir.join(&name);
    let temp = dir.join(format!(".{name}.tmp"));
    // `mode` only applies to a file this call creates, so a leftover from an
    // earlier crash could keep permissions we did not choose.
    let _ = fs::remove_file(&temp);

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    let mut file = options.open(&temp)?;
    file.write_all(report.as_bytes())?;
    file.sync_all()?;
    drop(file);

    fs::rename(&temp, &path)?;
    Ok(path)
}

fn prune_crash_reports(dir: &Path, keep: usize) {
    let mut reports = reports_in(dir);
    let Some(excess) = reports.len().checked_sub(keep) else {
        return;
    };
    reports.truncate(excess);
    for path in reports {
        let _ = fs::remove_file(path);
    }
}

/// Sorted ascending, which is chronological because the name is the timestamp.
fn reports_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut reports: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(CRASH_PREFIX) && name.ends_with(CRASH_SUFFIX))
        })
        .collect();
    reports.sort();
    reports
}

/// The log file, rotated by size. Cloning shares the one open file.
#[derive(Clone)]
pub struct RollingFile {
    path: Arc<Path>,
    inner: Arc<Mutex<Inner>>,
}

impl RollingFile {
    pub fn open(dir: &Path) -> io::Result<Self> {
        Self::with_cap(dir, MAX_BYTES)
    }

    fn with_cap(dir: &Path, cap: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;

        let path: Arc<Path> = Arc::from(dir.join(LOG_FILE).as_path());
        let file = open_log(&path, false)?;
        let len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);

        Ok(Self {
            path: Arc::clone(&path),
            inner: Arc::new(Mutex::new(Inner {
                path,
                file: Some(file),
                len,
                cap,
                reported: false,
            })),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl<'a> MakeWriter<'a> for RollingFile {
    type Writer = RollingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RollingWriter {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub struct RollingWriter {
    inner: Arc<Mutex<Inner>>,
}

/// Every result is `Ok`: a log line that cannot be written is not an error the
/// caller can do anything about, and `tracing` would only print about it.
impl io::Write for RollingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        lock(&self.inner).append(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        lock(&self.inner).flush();
        Ok(())
    }
}

struct Inner {
    path: Arc<Path>,
    /// `None` once a write or an open failed; the next write tries again.
    file: Option<File>,
    len: u64,
    cap: u64,
    /// One line on stderr per process, not one per dropped log line.
    reported: bool,
}

impl Inner {
    fn append(&mut self, buf: &[u8]) {
        let wanted = buf.len() as u64;
        // A write that failed left no file behind; picking the same one back up
        // is not a rotation, or a full disk would churn through every
        // generation one failed line at a time.
        if self.file.is_none() {
            self.reopen(false);
        }
        // An empty generation is never rotated: a single line longer than the
        // cap would otherwise rotate forever and still never fit.
        if self.len > 0 && self.len.saturating_add(wanted) > self.cap {
            self.rotate();
        }

        let Some(file) = self.file.as_mut() else {
            return;
        };
        match file.write_all(buf) {
            Ok(()) => self.len = self.len.saturating_add(wanted),
            Err(e) => {
                self.file = None;
                report(&mut self.reported, "cannot write the log file", &e);
            }
        }
    }

    fn flush(&mut self) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(e) = file.flush() {
            report(&mut self.reported, "cannot flush the log file", &e);
        }
    }

    /// Closes the live file before renaming — Windows refuses to rename a file
    /// that is still open — and reopens it empty, so the write that triggered
    /// this still lands.
    fn rotate(&mut self) {
        self.file = None;
        self.len = 0;

        for index in (1..KEEP).rev() {
            let from = self.generation(index);
            let to = self.generation(index + 1);
            shift(&mut self.reported, &from, &to);
        }
        let first = self.generation(1);
        let path = Arc::clone(&self.path);
        shift(&mut self.reported, &path, &first);

        self.reopen(true);
    }

    fn reopen(&mut self, truncate: bool) {
        match open_log(&self.path, truncate) {
            Ok(file) => {
                self.len = if truncate {
                    0
                } else {
                    file.metadata().map(|metadata| metadata.len()).unwrap_or(0)
                };
                self.file = Some(file);
            }
            Err(e) => {
                self.file = None;
                self.len = 0;
                report(&mut self.reported, "cannot open the log file", &e);
            }
        }
    }

    fn generation(&self, index: usize) -> PathBuf {
        let mut name = self.path.to_path_buf().into_os_string();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }
}

fn open_log(path: &Path, truncate: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true);
    if truncate {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

/// A generation that is not there yet is the normal case on a first rotation.
fn shift(reported: &mut bool, from: &Path, to: &Path) {
    match fs::rename(from, to) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => report(reported, "cannot rotate the log file", &e),
    }
}

fn report(reported: &mut bool, what: &str, error: &io::Error) {
    if !*reported {
        *reported = true;
        eprintln!("vorcall: {what}: {error}");
    }
}

/// A poisoned lock means some other thread panicked while logging; the data
/// behind it is still a valid `Inner`, and refusing to log would be worse.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tail of the log, in memory, for a crash report to quote.
#[derive(Clone, Default)]
pub struct Recent {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl Recent {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<String> {
        lock(&self.lines).iter().cloned().collect()
    }

    fn push(&self, line: String) {
        let mut lines = lock(&self.lines);
        if lines.len() == RECENT_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }
}

impl<S: Subscriber> Layer<S> for Recent {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);

        self.push(format!(
            "{} {} {}: {}{}",
            rfc3339(SystemTime::now()),
            metadata.level(),
            metadata.target(),
            visitor.message,
            visitor.fields
        ));
    }
}

#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: String,
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

/// UTC RFC-3339 to the second. `tracing_subscriber`'s own UTC timer needs its
/// `time` feature, which this workspace does not enable.
struct UtcTime;

impl FormatTime for UtcTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        write!(w, "{}", rfc3339(SystemTime::now()))
    }
}

fn rfc3339(time: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = parts(time);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The same instant as a file name: no colons, which Windows forbids.
fn stamp(time: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = parts(time);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// A clock before the epoch reads as the epoch; nothing here is worth failing
/// a crash report over.
fn parts(time: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);

    (
        year,
        month,
        day,
        (rest / 3_600) as u32,
        (rest % 3_600 / 60) as u32,
        (rest % 60) as u32,
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a civil date,
/// with the year shifted so March starts it and the leap day lands last.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;

    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    // January and February belong to the year after the shifted one.
    let year = year_of_era as i64 + era * 400 + i64::from(month <= 2);

    (year, month as u32, day)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::*;

    /// A directory of our own under the system temp dir; no test crate needed
    /// for something this small.
    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "vorcall-diagnostics-{}-{tag}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("creates the temporary directory");
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("reads the temporary directory")
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }

    /// `vorcall-app` is a binary-only crate, so its tracing target is the bin
    /// name `vorcall` — `vorcall_app` (the crate directory) would match nothing.
    #[test]
    fn the_file_filter_names_the_gui_binary_not_the_crate() {
        assert!(FILE_FILTER.contains("vorcall=debug"), "{FILE_FILTER}");
        assert!(!FILE_FILTER.contains("vorcall_app"), "{FILE_FILTER}");
    }

    #[test]
    fn the_log_rotates_and_keeps_three_generations() {
        let dir = temp_dir("rotate");
        let rolling = RollingFile::with_cap(&dir, 64).expect("opens the log");
        assert_eq!(rolling.path(), dir.join(LOG_FILE));

        // One `write_all` per line, 9 bytes each, the way the `fmt` layer
        // writes one formatted event: a 64-byte generation holds seven.
        let mut writer = rolling.make_writer();
        for line in 1..=40 {
            writer
                .write_all(format!("line-{line:03}\n").as_bytes())
                .expect("writes a line");
        }

        assert_eq!(
            names(&dir),
            vec![
                "vorcall.log".to_owned(),
                "vorcall.log.1".to_owned(),
                "vorcall.log.2".to_owned(),
                "vorcall.log.3".to_owned(),
            ]
        );

        let live = fs::read_to_string(dir.join(LOG_FILE)).expect("reads the live log");
        assert!(live.contains("line-040"), "{live}");
        assert!(!live.contains("line-001"), "{live}");
        for name in names(&dir) {
            let len = fs::metadata(dir.join(&name))
                .expect("reads a generation")
                .len();
            assert!(len <= 64, "{name} is {len} bytes");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_longer_than_the_cap_is_still_written() {
        let dir = temp_dir("oversized");
        let rolling = RollingFile::with_cap(&dir, 8).expect("opens the log");

        let mut writer = rolling.make_writer();
        writer
            .write_all(b"a line far longer than eight bytes\n")
            .expect("writes a line");

        let live = fs::read_to_string(dir.join(LOG_FILE)).expect("reads the live log");
        assert!(live.contains("far longer"), "{live}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_keeps_the_last_lines_only() {
        let recent = Recent::new();
        for line in 0..RECENT_LINES + 25 {
            recent.push(format!("line {line}"));
        }

        let snapshot = recent.snapshot();
        assert_eq!(snapshot.len(), RECENT_LINES);
        assert_eq!(snapshot.first().map(String::as_str), Some("line 25"));
        assert_eq!(
            snapshot.last().map(String::as_str),
            Some(format!("line {}", RECENT_LINES + 24).as_str())
        );
    }

    /// The reference values come from `date -u -d @<seconds>`.
    #[test]
    fn timestamps_are_utc() {
        let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);

        assert_eq!(rfc3339(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(at(1_234_567_890)), "2009-02-13T23:31:30Z");
        assert_eq!(rfc3339(at(1_757_000_000)), "2025-09-04T15:33:20Z");
        assert_eq!(stamp(at(1_757_000_000)), "20250904T153320Z");
    }

    fn crash_info() -> CrashInfo {
        CrashInfo {
            version: "0.4.0".to_owned(),
            platform: "linux-x86_64".to_owned(),
            at: UNIX_EPOCH + Duration::from_secs(1_757_000_000),
            thread: "voice".to_owned(),
            message: "the audio thread gave up".to_owned(),
            location: Some("crates/vorcall-app/src/voice.rs:12:5".to_owned()),
            backtrace: "0: vorcall_app::voice::run".to_owned(),
        }
    }

    #[test]
    fn a_crash_report_names_the_build_the_thread_and_the_last_lines() {
        let report = crash_report_text(
            &crash_info(),
            &["first line".to_owned(), "second line".to_owned()],
        );

        assert!(
            report.starts_with("vorcall 0.4.0 linux-x86_64\n"),
            "{report}"
        );
        assert!(report.contains("time: 2025-09-04T15:33:20Z"), "{report}");
        assert!(report.contains("thread: voice"), "{report}");
        assert!(report.contains("the audio thread gave up"), "{report}");
        assert!(
            report.contains("crates/vorcall-app/src/voice.rs:12:5"),
            "{report}"
        );
        assert!(report.contains("last log lines:"), "{report}");
        assert!(report.ends_with("first line\nsecond line\n"), "{report}");
    }

    #[test]
    fn crash_reports_are_pruned_oldest_first() {
        let dir = temp_dir("crashes");
        for day in 1..=6 {
            fs::write(dir.join(format!("crash-2020010{day}T000000Z.txt")), "old")
                .expect("writes an old report");
        }

        let fresh = write_crash_report(&dir, "fresh").expect("writes the report");
        assert_eq!(reports_in(&dir).len(), 7);
        // The temporary file was renamed, not left behind.
        assert_eq!(names(&dir).len(), 7);

        prune_crash_reports(&dir, KEEP_CRASHES);

        let left = reports_in(&dir);
        assert_eq!(left.len(), KEEP_CRASHES);
        assert!(left.contains(&fresh), "the newest report survives");
        assert!(!dir.join("crash-20200101T000000Z.txt").exists());
        assert!(!dir.join("crash-20200102T000000Z.txt").exists());
        assert!(dir.join("crash-20200103T000000Z.txt").exists());
        assert_eq!(
            fs::read_to_string(&fresh).expect("reads the report"),
            "fresh"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_crash_report_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("modes");
        let path = write_crash_report(&dir, "fresh").expect("writes the report");

        let mode = fs::metadata(&path)
            .expect("reads the report")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the report is readable by {mode:o}");

        let _ = fs::remove_dir_all(&dir);
    }
}
