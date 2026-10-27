#![windows_subsystem = "windows"]

mod audio;
mod cli;
mod config;
mod dsp;
#[cfg(feature = "noise-cancellation")]
mod noise_cancellation;
mod startup;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{Cli, Command};
use std::{
    io::{Seek, SeekFrom, Write},
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE},
    Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT},
    System::Threading::{CreateMutexW, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            MOD_NOREPEAT, MOD_WIN, RegisterHotKey, UnregisterHotKey, VK_OEM_2,
        },
        WindowsAndMessaging::*,
    },
};

const MENU_MESSAGE: u32 = WM_APP + 1;
const AUDIO_ERROR_MESSAGE: u32 = WM_APP + 2;
const HOTKEY_ID: i32 = 1;
static LOG_ENABLED: AtomicBool = AtomicBool::new(false);

fn log(message: &str) {
    if !LOG_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    const MAX_BYTES: usize = 1024 * 1024;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let Ok(_guard) = LOCK.lock() else { return };
    let path = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Sidetone");
    let _ = std::fs::create_dir_all(&path);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path.join("sidetone.log"))
    {
        let elapsed = STARTED
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_secs_f32();
        let mut line = format!("[{} +{elapsed:.3}s] {message}", std::process::id());
        let mut end = line.len().min(MAX_BYTES - 1);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push('\n');
        let Ok(metadata) = file.metadata() else {
            return;
        };
        if metadata.len().saturating_add(line.len() as u64) > MAX_BYTES as u64
            && file.set_len(0).is_err()
        {
            return;
        }
        if file.seek(SeekFrom::End(0)).is_ok() {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

struct Instance(HANDLE);
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn main() {
    // Borrow the launching terminal for CLI help and output.
    // At login there is no parent console, so Sidetone stays windowless.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
    if let Err(error) = run(Cli::parse()) {
        eprintln!("sidetone: {error:#}");
        log(&format!("Sidetone failed: {error:#}"));
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    if let Some(Command::Startup { remove }) = cli.command {
        if remove {
            println!("removed startup entry ({})", startup::uninstall()?);
        } else {
            let exe = std::env::current_exe().context("finding the Sidetone executable")?;
            println!("registered Sidetone at login ({})", startup::install(&exe)?);
            println!("takes effect at your next login");
        }
        return Ok(());
    }
    let recovery_test = cli.reconnect_test;
    let smoke_test = recovery_test || cli.smoke_test;
    let name: Vec<u16> = "Local\\Sidetone.AudioMonitor\0".encode_utf16().collect();
    let handle = unsafe { CreateMutexW(null_mut(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let existing = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let _instance = Instance(handle);
    if existing {
        bail!("Sidetone is already running");
    }
    LOG_ENABLED.store(cli.log, Ordering::Relaxed);

    // Create this thread's message queue before any callbacks can post to it.
    let thread_id = unsafe { GetCurrentThreadId() };
    let mut message: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
    }
    let (menu_tx, menu_rx) = std::sync::mpsc::channel();
    let test_menu_tx = menu_tx.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_tx.send(event);
        unsafe {
            PostThreadMessageW(thread_id, MENU_MESSAGE, 0, 0);
        }
    }));
    let (error_tx, error_rx) = std::sync::mpsc::channel();
    let on_event: Arc<dyn Fn(audio::Event) + Send + Sync> = Arc::new(move |error| {
        let _ = error_tx.send(error);
        unsafe {
            PostThreadMessageW(thread_id, AUDIO_ERROR_MESSAGE, 0, 0);
        }
    });

    let mut config = if smoke_test {
        config::Config {
            delay_ms: 375,
            volume_percent: 137,
        }
    } else {
        config::Config::load().unwrap_or_else(|error| {
            log(&format!(
                "Could not load settings; using defaults: {error:#}"
            ));
            config::Config::default()
        })
    };
    let mut state = dsp::MonitorState::new();
    state.volume_percent = config.volume_percent;
    log(&format!(
        "Settings: delay={} ms, volume={}%",
        config.delay_ms, config.volume_percent
    ));
    if smoke_test {
        state.enabled = false;
    }
    let gain = Arc::new(AtomicU32::new(state.gain().to_bits()));
    let delay_ms = Arc::new(AtomicU32::new(config.delay_ms));
    let _audio = audio::Controller::start(gain.clone(), delay_ms.clone(), on_event, recovery_test)?;
    let mut counters: Option<Arc<audio::Counters>> = None;
    let mut connections = 0;
    let menu = Menu::new();
    let enabled = CheckMenuItem::new("On", true, state.enabled, None);
    let delay_menu = Submenu::new("Delay", true);
    let delay_items: Vec<_> = dsp::DELAY_PRESETS
        .iter()
        .map(|&ms| {
            (
                ms,
                CheckMenuItem::new(
                    format!("{ms} ms"),
                    true,
                    ms == delay_ms.load(Ordering::Relaxed),
                    None,
                ),
            )
        })
        .collect();
    for (_, item) in &delay_items {
        delay_menu.append(item)?;
    }
    let volume_menu = Submenu::new("Volume", true);
    let volume_items: Vec<_> = dsp::VOLUME_PRESETS
        .iter()
        .map(|&percent| {
            (
                percent,
                CheckMenuItem::new(
                    format!("{percent}%"),
                    true,
                    percent == state.volume_percent,
                    None,
                ),
            )
        })
        .collect();
    for (_, item) in &volume_items {
        volume_menu.append(item)?;
    }
    if smoke_test
        && delay_items
            .iter()
            .chain(&volume_items)
            .any(|(_, item)| item.is_checked())
    {
        bail!("Smoke test: custom settings incorrectly checked a preset");
    }
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[
        &enabled,
        &delay_menu,
        &volume_menu,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;
    let icon_image =
        image::load_from_memory(include_bytes!("../assets/sidetone.png"))?.into_rgba8();
    let (width, height) = icon_image.dimensions();
    let icon = Icon::from_rgba(icon_image.into_raw(), width, height)?;
    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_tooltip("Sidetone")
        .build()?;
    let hotkey_registered = unsafe {
        RegisterHotKey(
            null_mut(),
            HOTKEY_ID,
            MOD_NOREPEAT | MOD_WIN,
            VK_OEM_2 as u32,
        )
    } != 0;
    if !hotkey_registered {
        log("Win+/ is already in use or unavailable; tray toggle remains available.");
    } else {
        log("Win+/ registered");
    }
    let refresh = |state: &dsp::MonitorState, available: bool| {
        enabled.set_checked(state.enabled);
        let _ = tray.set_tooltip(Some(if !available {
            "Sidetone: reconnecting audio"
        } else if state.enabled {
            "Sidetone: On"
        } else {
            "Sidetone: Off"
        }));
    };
    refresh(&state, counters.is_some());
    log(if smoke_test {
        "Muted tray/audio smoke test started"
    } else {
        "Sidetone tray started"
    });
    if smoke_test {
        let test_presets: Vec<_> = [(0, 0), (200, 100), (100, 200)]
            .iter()
            .map(|(ms, percent)| {
                (
                    delay_items
                        .iter()
                        .find(|(preset, _)| preset == ms)
                        .unwrap()
                        .1
                        .id()
                        .clone(),
                    volume_items
                        .iter()
                        .find(|(preset, _)| preset == percent)
                        .unwrap()
                        .1
                        .id()
                        .clone(),
                )
            })
            .collect();
        std::thread::spawn(move || {
            if recovery_test {
                std::thread::sleep(Duration::from_secs(20));
            } else {
                // Exercise the same menu handler as real clicks, including both extremes.
                for (delay, volume) in test_presets {
                    std::thread::sleep(Duration::from_secs(3));
                    let _ = test_menu_tx.send(MenuEvent { id: delay });
                    let _ = test_menu_tx.send(MenuEvent { id: volume });
                    unsafe {
                        PostThreadMessageW(thread_id, MENU_MESSAGE, 0, 0);
                    }
                }
                std::thread::sleep(Duration::from_secs(3));
            }
            unsafe {
                PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
            }
        });
    }
    let mut loop_error = None;
    loop {
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result == 0 {
            break;
        }
        if result == -1 {
            loop_error = Some(std::io::Error::last_os_error());
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        // Dispatch can enter the tray menu's modal message loop, which can eat
        // thread wakeups. Drain both channels after dispatch regardless of the
        // message type so events received while the menu was open survive.
        let mut toggle = message.message == WM_HOTKEY && message.wParam == HOTKEY_ID as usize;
        let mut exit = false;
        for event in menu_rx.try_iter() {
            if event.id == *quit.id() {
                exit = true;
            }
            if event.id == *enabled.id() {
                toggle = !toggle;
            }
            if let Some((ms, _)) = delay_items.iter().find(|(_, item)| event.id == *item.id()) {
                delay_ms.store(*ms, Ordering::Relaxed);
                config.delay_ms = *ms;
                for (preset, item) in &delay_items {
                    item.set_checked(preset == ms);
                }
                if !smoke_test && let Err(error) = config.save() {
                    log(&format!("Could not save settings: {error:#}"));
                }
                log(&format!("Delay selected: {ms} ms"));
            }
            if let Some((percent, _)) = volume_items.iter().find(|(_, item)| event.id == *item.id())
            {
                state.volume_percent = *percent;
                config.volume_percent = *percent;
                gain.store(state.gain().to_bits(), Ordering::Relaxed);
                for (preset, item) in &volume_items {
                    item.set_checked(preset == percent);
                }
                if !smoke_test && let Err(error) = config.save() {
                    log(&format!("Could not save settings: {error:#}"));
                }
                log(&format!("Volume selected: {percent}%"));
            }
        }
        if exit {
            break;
        }
        for event in error_rx.try_iter() {
            match event {
                audio::Event::Ready(current) => {
                    counters = Some(current);
                    connections += 1;
                }
                audio::Event::Waiting => {
                    counters = None;
                }
            }
            refresh(&state, counters.is_some());
        }
        if toggle && !smoke_test {
            let was_enabled = state.enabled;
            state.toggle();
            gain.store(state.gain().to_bits(), Ordering::Relaxed);
            refresh(&state, counters.is_some());
            if state.enabled != was_enabled {
                // Static embedded WAVs remain valid for asynchronous playback.
                // Separate playback keeps the Off cue audible after monitoring mutes.
                let sound: &'static [u8] = if state.enabled {
                    include_bytes!("../assets/on.wav")
                } else {
                    include_bytes!("../assets/off.wav")
                };
                let played = unsafe {
                    PlaySoundW(
                        sound.as_ptr().cast(),
                        null_mut(),
                        SND_ASYNC | SND_MEMORY | SND_NODEFAULT,
                    )
                };
                if played == 0 {
                    log("Toggle sound could not be played");
                }
            }
        }
    }
    if hotkey_registered {
        unsafe {
            UnregisterHotKey(null_mut(), HOTKEY_ID);
        }
    }
    MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
    if let Some(error) = loop_error {
        return Err(error.into());
    }
    if smoke_test {
        if gain.load(Ordering::Relaxed) != 0.0f32.to_bits() {
            bail!("Smoke test: changing volume unexpectedly unmuted playback");
        }
        let counters = counters
            .as_ref()
            .context("Smoke test: audio did not remain running")?;
        if recovery_test && connections < 3 {
            bail!("Reconnect test: expected three connected generations, got {connections}");
        }
        log(&format!("Audio connections during test: {connections}"));
        let captured = counters.captured.load(Ordering::Relaxed);
        let rendered = counters.rendered.load(Ordering::Relaxed);
        log(&format!(
            "Smoke test: captured={captured}, rendered={rendered}, underrun frames={}, overrun frames={}",
            counters.underruns.load(Ordering::Relaxed),
            counters.overruns.load(Ordering::Relaxed)
        ));
        let processed = counters.processed.load(Ordering::Relaxed);
        log(&format!(
            "Processing smoke test: processed={processed}, max processing={} us, late samples={}",
            counters.processing_max_us.load(Ordering::Relaxed),
            counters.late.load(Ordering::Relaxed)
        ));
        let scheduled = counters.scheduled.load(Ordering::Relaxed);
        log(&format!(
            "Scheduled processed playback: {scheduled}/{rendered} frames"
        ));
        if scheduled < rendered * 9 / 10 {
            bail!("Smoke test: too little processed audio reached scheduled playback");
        }
        if captured == 0 || rendered == 0 || processed == 0 {
            bail!("Smoke test: audio callbacks did not run");
        }
        log("Smoke test passed (output muted)");
    }
    Ok(())
}
