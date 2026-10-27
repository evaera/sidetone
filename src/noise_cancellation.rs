mod cache;

use anyhow::{Context, Result, bail};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::windows::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

use crate::dsp::SAMPLES;
// Approximate algorithmic latency of the supported Noise cancellation module.
pub const FILTER_DELAY: Duration = Duration::from_nanos(26_687_500);

pub struct Helper {
    child: Child,
    socket: Option<TcpStream>,
}
impl Drop for Helper {
    fn drop(&mut self) {
        self.socket.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Helper {
    pub fn start(cancel: Option<&AtomicBool>) -> Result<Self> {
        let root = cache::prepare(cancel)?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let mut random = [0u8; 32];
        if unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                random.as_mut_ptr(),
                32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        } < 0
        {
            bail!("Cannot generate helper authentication token");
        }
        let token: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let _ = std::fs::remove_file(root.join("helper-error.txt"));
        let child = Command::new(root.join("runtime/Discord.exe"))
            .arg("--disable-gpu")
            .current_dir(&root)
            .env("SIDETONE_NOISE_CANCELLATION_ROOT", &root)
            .env(
                "SIDETONE_NOISE_CANCELLATION_PORT",
                listener.local_addr()?.port().to_string(),
            )
            .env("SIDETONE_NOISE_CANCELLATION_TOKEN", &token)
            .env_remove("ELECTRON_RUN_AS_NODE")
            .env_remove("NODE_OPTIONS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .context("Start cached Noise cancellation helper")?;
        let mut helper = Self {
            child,
            socket: None,
        };
        crate::log(&format!(
            "Noise cancellation helper starting: PID {}",
            helper.child.id()
        ));
        let started = Instant::now();
        loop {
            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                bail!("Noise cancellation startup cancelled");
            }
            if let Some(status) = helper.child.try_wait()? {
                let detail =
                    std::fs::read_to_string(root.join("helper-error.txt")).unwrap_or_default();
                bail!("Noise cancellation helper exited {status}: {detail}");
            }
            if started.elapsed() > Duration::from_secs(10) {
                bail!("Noise cancellation helper startup timed out");
            }
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket.set_nonblocking(false)?;
                    socket.set_nodelay(true)?;
                    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
                    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
                    let mut hello = [0u8; 68];
                    if socket.read_exact(&mut hello).is_ok()
                        && &hello[..64] == token.as_bytes()
                        && &hello[64..] == b"SK01"
                    {
                        helper.socket = Some(socket);
                        crate::log(&format!(
                            "Noise cancellation ready: helper PID {}, 48 kHz / 10 ms",
                            helper.child.id()
                        ));
                        return Ok(helper);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    pub fn process(&mut self, input: &[f32; SAMPLES]) -> Result<[f32; SAMPLES]> {
        let socket = self.socket.as_mut().context("helper disconnected")?;
        let mut bytes = [0u8; SAMPLES * 4];
        for (sample, bytes) in input.iter().zip(bytes.chunks_exact_mut(4)) {
            bytes.copy_from_slice(&sample.to_le_bytes());
        }
        socket
            .write_all(&bytes)
            .context("Send audio to Noise cancellation")?;
        socket
            .read_exact(&mut bytes)
            .context("Receive audio from Noise cancellation")?;
        let output = std::array::from_fn(|i| {
            f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        });
        if !output.iter().all(|s| s.is_finite()) {
            bail!("Noise cancellation returned invalid samples");
        }
        Ok(output)
    }
}
