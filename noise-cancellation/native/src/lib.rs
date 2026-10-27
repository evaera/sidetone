use std::{
    ffi::c_void,
    io::{Read, Write},
    net::TcpStream,
    os::windows::ffi::OsStrExt,
    time::Duration,
};

// Keep the addon dependency-free so the application build can compile and embed it.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(path: *const u16) -> *mut c_void;
    fn GetProcAddress(
        module: *mut c_void,
        name: *const u8,
    ) -> Option<unsafe extern "system" fn() -> isize>;
}

struct Session(*mut c_void, unsafe extern "system" fn(*mut c_void));
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            (self.1)(self.0);
        }
    }
}

// This private ABI is hash-checked by host.cjs before this addon is loaded.
fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let path: Vec<u16> = std::env::var_os("SIDETONE_NOISE_CANCELLATION_MODULE")
        .ok_or("module missing")?
        .encode_wide()
        .chain(Some(0))
        .collect();
    let library = unsafe { LoadLibraryW(path.as_ptr()) };
    if library.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let initialize = unsafe { GetProcAddress(library, c"KrispInitializeExternal".as_ptr().cast()) }
        .ok_or("initialize missing")?;
    let initialize: unsafe extern "system" fn() -> i32 = unsafe { std::mem::transmute(initialize) };
    let setup = unsafe { GetProcAddress(library, c"KrispNCSetup2".as_ptr().cast()) }
        .ok_or("setup missing")?;
    let setup: unsafe extern "system" fn(i32, i32, bool, bool) -> *mut c_void =
        unsafe { std::mem::transmute(setup) };
    let process = unsafe { GetProcAddress(library, c"KrispNCProcessFloat".as_ptr().cast()) }
        .ok_or("process missing")?;
    let process: unsafe extern "system" fn(*mut c_void, *const f32, usize, *mut f32, usize) -> i32 =
        unsafe { std::mem::transmute(process) };
    let reset = unsafe { GetProcAddress(library, c"KrispNCReset".as_ptr().cast()) }
        .ok_or("reset missing")?;
    let reset: unsafe extern "system" fn(*mut c_void) = unsafe { std::mem::transmute(reset) };
    if unsafe { initialize() } != 0 {
        return Err("Noise cancellation initialization failed".into());
    }
    let session = unsafe { setup(48_000, 10, false, true) };
    if session.is_null() {
        return Err("Noise cancellation setup failed".into());
    }
    let session = Session(session, reset);
    let port: u16 = std::env::var("SIDETONE_NOISE_CANCELLATION_PORT")?.parse()?;
    let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let token = std::env::var("SIDETONE_NOISE_CANCELLATION_TOKEN")?;
    if token.len() != 64 {
        return Err("invalid token".into());
    }
    socket.write_all(token.as_bytes())?;
    socket.write_all(b"SK01")?;
    let mut bytes = [0u8; 480 * 4];
    let mut input = [0.0f32; 480];
    let mut output = [0.0f32; 480];
    loop {
        match socket.read_exact(&mut bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        for (sample, bytes) in input.iter_mut().zip(bytes.chunks_exact(4)) {
            *sample = f32::from_le_bytes(bytes.try_into()?);
        }
        if !input.iter().all(|s| s.is_finite()) {
            return Err("invalid input".into());
        }
        output.fill(f32::NAN);
        let status = unsafe { process(session.0, input.as_ptr(), 480, output.as_mut_ptr(), 480) };
        if status != 0 || !output.iter().all(|s| s.is_finite()) {
            return Err(format!("Noise cancellation processing failed: {status}").into());
        }
        for (sample, bytes) in output.iter().zip(bytes.chunks_exact_mut(4)) {
            bytes.copy_from_slice(&sample.to_le_bytes());
        }
        socket.write_all(&bytes)?;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn napi_register_module_v1(_env: *mut c_void, exports: *mut c_void) -> *mut c_void {
    let error = match std::panic::catch_unwind(serve) {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("Rust helper panic".to_owned()),
    };
    if let Some(error) = error {
        if let Some(root) = std::env::var_os("SIDETONE_NOISE_CANCELLATION_ROOT") {
            let _ = std::fs::write(
                std::path::PathBuf::from(root).join("helper-error.txt"),
                error,
            );
        }
        std::process::exit(1);
    }
    exports
}
