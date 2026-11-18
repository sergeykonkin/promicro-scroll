use std::{
    cell::RefCell,
    fs::{self, File},
    io::{self, Write},
    path::PathBuf,
    ptr::null_mut,
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::WindowsAndMessaging::*,
};

#[derive(Clone, Copy, Debug)]
struct Sample {
    windows_ms: u32,
    received_us: u128,
    delta: i16,
}

struct Capture {
    sender: Sender<Sample>,
    started: Instant,
}

thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

// Windows calls this on the thread that owns the hook. Keep disk I/O and
// statistics on the consumer thread so this callback returns immediately.
unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && message == WM_MOUSEWHEEL as usize {
        // The pointer is valid for this callback, as specified by WH_MOUSE_LL.
        let event = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        let delta = (event.mouseData >> 16) as u16 as i16;
        if event.flags & LLMHF_INJECTED == 0 && delta != 0 {
            CAPTURE.with(|capture| {
                if let Some(capture) = capture.borrow().as_ref() {
                    let sample = Sample {
                        windows_ms: event.time,
                        received_us: capture.started.elapsed().as_micros(),
                        delta,
                    };
                    if capture.sender.send(sample).is_err() {
                        unsafe { PostQuitMessage(1) };
                    }
                }
            });
        }
    }
    // Never consume the event: normal scrolling continues in other apps.
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

#[derive(Debug)]
struct Event {
    sample: Sample,
    elapsed_us: u128,
    interval_us: Option<u128>,
    transition: &'static str,
}

// Instant is monotonic. Keep integer microseconds until presentation; Windows
// message timestamps remain diagnostic data and never determine replay timing.
fn next_event(previous: Option<&Event>, sample: Sample) -> io::Result<Event> {
    let interval_us = previous
        .map(|p| {
            sample
                .received_us
                .checked_sub(p.sample.received_us)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "received_us moved backwards")
                })
        })
        .transpose()?;
    Ok(Event {
        sample,
        elapsed_us: previous.map_or(0, |p| p.elapsed_us + interval_us.unwrap()),
        interval_us,
        transition: match previous {
            None => "first",
            Some(p) if p.sample.delta.signum() == sample.delta.signum() => "continue",
            Some(_) => "reverse",
        },
    })
}

fn direction(delta: i16) -> &'static str {
    if delta > 0 { "up" } else { "down" }
}

fn stats_json(values: &[u128]) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::json!({"count": 0, "min_ms": null, "median_ms": null,
            "p95_ms": null, "max_ms": null, "mean_ms": null});
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let median_ms = (sorted[(n - 1) / 2] as f64 + sorted[n / 2] as f64) / 2000.0;
    let mean_ms = sorted.iter().map(|v| *v as f64).sum::<f64>() / n as f64 / 1000.0;
    serde_json::json!({"count": n, "min_ms": sorted[0] as f64 / 1000.0,
        "median_ms": median_ms, "p95_ms": sorted[(95 * n).div_ceil(100) - 1] as f64 / 1000.0,
        "max_ms": sorted[n - 1] as f64 / 1000.0, "mean_ms": mean_ms})
}

fn intervals(events: &[Event], transition: &str, dir: &str) -> Vec<u128> {
    events
        .iter()
        .filter(|e| e.transition == transition && direction(e.sample.delta) == dir)
        .filter_map(|e| e.interval_us)
        .collect()
}

fn summary_json(events: &[Event], duration_ms: u128, started_unix_ms: u128) -> String {
    let up = intervals(events, "continue", "up");
    let down = intervals(events, "continue", "down");
    let to_up = intervals(events, "reverse", "up");
    let to_down = intervals(events, "reverse", "down");
    let up_count = events.iter().filter(|e| e.sample.delta > 0).count();
    let net_delta: i64 = events.iter().map(|e| i64::from(e.sample.delta)).sum();
    let nonstandard = events.iter().filter(|e| e.sample.delta % 120 != 0).count();
    let first = events
        .first()
        .map_or("null".into(), |e| e.sample.received_us.to_string());
    let last = events
        .last()
        .map_or("null".into(), |e| e.sample.received_us.to_string());
    format!(
        concat!(
            "{{\n  \"schema_version\": 2,\n  \"timing_source\": \"monotonic_callback\",\n  \"started_unix_ms\": {started_unix_ms},\n",
            "  \"recording_duration_ms\": {duration_ms},\n  \"event_count\": {count},\n",
            "  \"up_events\": {up_count},\n  \"down_events\": {down_count},\n",
            "  \"reversals\": {reversals},\n  \"net_delta\": {net_delta},\n",
            "  \"events_not_multiple_of_120\": {nonstandard},\n",
            "  \"first_received_us\": {first},\n  \"last_received_us\": {last},\n",
            "  \"continue_up\": {up},\n  \"continue_down\": {down},\n",
            "  \"reverse_down_to_up\": {to_up},\n  \"reverse_up_to_down\": {to_down}\n}}\n"
        ),
        count = events.len(),
        down_count = events.len() - up_count,
        reversals = to_up.len() + to_down.len(),
        up = stats_json(&up),
        down = stats_json(&down),
        to_up = stats_json(&to_up),
        to_down = stats_json(&to_down),
        started_unix_ms = started_unix_ms,
        duration_ms = duration_ms,
        up_count = up_count,
        net_delta = net_delta,
        nonstandard = nonstandard,
        first = first,
        last = last,
    )
}

const CSV_HEADER: &str =
    "index,elapsed_us,interval_us,delta,notches,direction,transition,windows_time_ms,received_us";

fn write_event(file: &mut impl Write, index: usize, event: &Event) -> io::Result<()> {
    let sample = event.sample;
    writeln!(
        file,
        "{},{},{},{},{},{},{},{},{}",
        index,
        event.elapsed_us,
        event.interval_us.map_or(String::new(), |v| v.to_string()),
        sample.delta,
        f64::from(sample.delta) / 120.0,
        direction(sample.delta),
        event.transition,
        sample.windows_ms,
        sample.received_us
    )
}

fn consume(receiver: mpsc::Receiver<Sample>, mut file: Option<File>) -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    for sample in receiver {
        let event = next_event(events.last(), sample)?;
        if let Some(file) = file.as_mut() {
            write_event(file, events.len(), &event)?;
            // File is unbuffered: each complete CSV row survives a normal process abort.
        }
        events.push(event);
    }
    if let Some(file) = file.as_mut() {
        file.sync_all()?;
    }
    Ok(events)
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!(
            "scroll-reader [--seconds N | --check]\n\nRecords vertical mouse wheel events globally.\nDefault: press Enter to stop and save.\n--seconds N: stop automatically after N seconds.\n--check: install and remove the hook without saving files."
        );
        return Ok(());
    }
    let check = args.as_slice() == ["--check"];
    let seconds = if check {
        Some(0.2)
    } else if args.len() == 2 && args[0] == "--seconds" {
        let seconds: f64 = args[1].parse()?;
        if !seconds.is_finite() || !(0.01..=86400.0).contains(&seconds) {
            return Err("Seconds must be between 0.01 and 86400".into());
        }
        Some(seconds)
    } else if args.is_empty() {
        None
    } else {
        return Err("Unknown arguments. Use --help.".into());
    };

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let directory = if check {
        None
    } else {
        fs::create_dir_all("recordings")?;
        let path = PathBuf::from(format!("recordings/session-{}", stamp.as_nanos()));
        fs::create_dir(&path)?;
        Some(path)
    };
    let file = if let Some(path) = &directory {
        let mut file = File::create_new(path.join("events.csv"))?;
        writeln!(file, "{CSV_HEADER}")?;
        Some(file)
    } else {
        None
    };
    let (sender, receiver) = mpsc::channel();
    let started = Instant::now();
    CAPTURE.with(|capture| *capture.borrow_mut() = Some(Capture { sender, started }));
    // PeekMessage creates this thread's queue before another thread posts WM_QUIT.
    let mut message = MSG::default();
    unsafe {
        PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
    }
    let hook = unsafe {
        SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook),
            GetModuleHandleW(null_mut()),
            0,
        )
    };
    if hook.is_null() {
        CAPTURE.with(|capture| *capture.borrow_mut() = None);
        return Err(io::Error::last_os_error().into());
    }
    let writer = thread::spawn(move || consume(receiver, file));
    let thread_id = unsafe { GetCurrentThreadId() };
    println!("Recording vertical wheel events. Normal scrolling remains active.");
    println!("Use only the mouse you want to sample. Software-injected events are ignored.");
    if let Some(seconds) = seconds {
        println!("Stopping automatically after {seconds} seconds.");
    } else {
        println!("Scroll naturally up and down, then press Enter here to stop and save.");
    }
    thread::spawn(move || {
        if let Some(seconds) = seconds {
            thread::sleep(Duration::from_secs_f64(seconds));
        } else {
            let mut line = String::new();
            if let Err(error) = io::stdin().read_line(&mut line) {
                eprintln!("Cannot read console input: {error}. Stopping capture.");
            }
        }
        if unsafe { PostThreadMessageW(thread_id, WM_QUIT, 0, 0) } == 0 {
            eprintln!("Cannot stop capture: {}", io::Error::last_os_error());
        }
    });
    let loop_error = loop {
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result == -1 {
            break Some(io::Error::last_os_error());
        }
        if result == 0 {
            break None;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    };
    let duration_ms = started.elapsed().as_millis();
    let unhooked = unsafe { UnhookWindowsHookEx(hook) };
    let unhook_error = (unhooked == 0).then(io::Error::last_os_error);
    CAPTURE.with(|capture| *capture.borrow_mut() = None);
    let events = writer.join().map_err(|_| "Recording thread panicked")??;
    if let Some(path) = directory {
        let summary = summary_json(&events, duration_ms, stamp.as_millis());
        fs::write(path.join("summary.json"), summary)?;
        println!(
            "Saved {} events to {}",
            events.len(),
            fs::canonicalize(path)?.display()
        );
        if events.is_empty() {
            println!("No wheel events captured. Record another sample before creating firmware.");
        }
    } else {
        println!(
            "Hook check passed ({} events observed, none saved).",
            events.len()
        );
    }
    if let Some(error) = loop_error.or(unhook_error) {
        return Err(error.into());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence(samples: &[(u128, i16)]) -> Vec<Event> {
        let mut events = Vec::new();
        for &(received_us, delta) in samples {
            events.push(
                next_event(
                    events.last(),
                    Sample {
                        windows_ms: 100,
                        delta,
                        received_us,
                    },
                )
                .unwrap(),
            );
        }
        events
    }

    #[test]
    fn separates_continuations_and_both_reversal_directions() {
        let events = sequence(&[
            (100000, 120),
            (115123, 240),
            (500456, -120),
            (520789, -120),
            (900999, 120),
        ]);
        assert_eq!(events[0].interval_us, None);
        assert_eq!(events[0].transition, "first");
        assert_eq!(intervals(&events, "continue", "up"), vec![15123]);
        assert_eq!(intervals(&events, "continue", "down"), vec![20333]);
        assert_eq!(intervals(&events, "reverse", "down"), vec![385333]);
        assert_eq!(intervals(&events, "reverse", "up"), vec![380210]);
        assert_eq!(events[4].elapsed_us, 800999);
        assert_eq!(events[1].sample.delta, 240);
    }

    #[test]
    fn retains_submillisecond_intervals_real_zeroes_and_long_pauses() {
        let events = sequence(&[(1000, 120), (1421, 120), (1421, -120), (60001421, -120)]);
        assert_eq!(events[1].interval_us, Some(421));
        assert_eq!(events[2].interval_us, Some(0));
        assert_eq!(events[3].interval_us, Some(60000000));
        assert_eq!(events[3].elapsed_us, 60000421);
    }

    #[test]
    fn statistics_handle_empty_single_and_even_samples() {
        assert!(stats_json(&[])["median_ms"].is_null());
        assert_eq!(stats_json(&[7421])["p95_ms"], 7.421);
        let stats = stats_json(&[40000, 10000, 30000, 20000]);
        assert_eq!(stats["median_ms"], 25.0);
        assert_eq!(stats["mean_ms"], 25.0);
        assert_eq!(stats["p95_ms"], 40.0);
    }

    #[test]
    fn callback_timing_ignores_equal_or_wrapped_windows_timestamps() {
        let first = next_event(
            None,
            Sample {
                windows_ms: u32::MAX,
                received_us: 567454,
                delta: 120,
            },
        )
        .unwrap();
        let second = next_event(
            Some(&first),
            Sample {
                windows_ms: u32::MAX,
                received_us: 580459,
                delta: 120,
            },
        )
        .unwrap();
        let third = next_event(
            Some(&second),
            Sample {
                windows_ms: 9,
                received_us: 654460,
                delta: -120,
            },
        )
        .unwrap();
        assert_eq!(second.interval_us, Some(13005));
        assert_eq!(third.interval_us, Some(74001));
        assert_eq!(third.elapsed_us, 87006);
    }
}
