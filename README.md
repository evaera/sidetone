# Sidetone

Windows x64 tray app for hearing your microphone through your default playback device.

## Controls

- **Win+/** or **On** in the tray menu toggles monitoring.
- **Delay:** 0–200 ms in 25 ms steps; default 0 ms.
- **Volume:** 0–200% in 25% steps; default 100%.
- **Quit:** exit.

Starts enabled, saves delay and volume, and reconnects when audio devices change. A 0 ms delay uses the lowest available latency.

## Build

```text
cargo build --release --locked
target\release\sidetone.exe
```

For noise cancellation, enable the feature:

```text
cargo build --release --locked --features noise-cancellation
```

Quit Sidetone before rebuilding.

## Start at login

```text
sidetone.exe startup
```

Remove with `sidetone.exe startup --remove`. Register again if you move the executable.

## Settings

`%APPDATA%\Sidetone\config.toml`:

```toml
delay_ms = 0
volume_percent = 100
```

The file accepts custom nonnegative integers. Non-preset values leave the menu unchecked. Tray changes save automatically; restart after editing the file.

Use `--log` for diagnostics in `%LOCALAPPDATA%\Sidetone\sidetone.log` (resets at 1 MiB).

## License

[MIT](LICENSE)
