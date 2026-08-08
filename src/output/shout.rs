//! Minimal, safe bindings to libshout (>= 2.4).
//!
//! libshout handles the Icecast source protocol (HTTP PUT + ICY), real-time
//! pacing (it parses the encoded bitstream for timing) and metadata updates
//! (`/admin/metadata`). Encoding of raw PCM is done in Rust — libshout only
//! transports already-encoded data.

use std::ffi::{c_char, c_int, c_uint, c_uchar, c_ushort, CString};
use std::ptr;
use std::sync::Once;

use crate::config::{OutputConfig, OutputFormat};
use crate::Result;

// `shout.h` constants.
const SHOUT_FORMAT_OGG: c_uint = 0;
const SHOUT_FORMAT_MP3: c_uint = 1;
const SHOUTERR_SUCCESS: c_int = 0;

#[repr(C)]
struct ShoutRaw {
    _unused: [u8; 1],
}
#[repr(C)]
struct ShoutMetadataRaw {
    _unused: [u8; 1],
}

#[link(name = "shout")]
extern "C" {
    fn shout_init();
    fn shout_new() -> *mut ShoutRaw;
    fn shout_free(self_: *mut ShoutRaw);
    fn shout_set_host(self_: *mut ShoutRaw, value: *const c_char) -> c_int;
    fn shout_set_port(self_: *mut ShoutRaw, value: c_ushort) -> c_int;
    fn shout_set_user(self_: *mut ShoutRaw, value: *const c_char) -> c_int;
    fn shout_set_password(self_: *mut ShoutRaw, value: *const c_char) -> c_int;
    fn shout_set_mount(self_: *mut ShoutRaw, value: *const c_char) -> c_int;
    fn shout_set_format(self_: *mut ShoutRaw, value: c_uint) -> c_int;
    fn shout_set_audio_info(self_: *mut ShoutRaw, name: *const c_char, value: *const c_char)
        -> c_int;
    fn shout_set_meta(self_: *mut ShoutRaw, name: *const c_char, value: *const c_char) -> c_int;
    fn shout_set_public(self_: *mut ShoutRaw, value: c_uint) -> c_int;
    fn shout_set_agent(self_: *mut ShoutRaw, value: *const c_char) -> c_int;
    fn shout_open(self_: *mut ShoutRaw) -> c_int;
    fn shout_close(self_: *mut ShoutRaw) -> c_int;
    fn shout_send(self_: *mut ShoutRaw, data: *const c_uchar, len: usize) -> c_int;
    fn shout_sync(self_: *mut ShoutRaw);
    fn shout_delay(self_: *mut ShoutRaw) -> c_int;
    fn shout_get_error(self_: *mut ShoutRaw) -> *const c_char;
    fn shout_metadata_new() -> *mut ShoutMetadataRaw;
    fn shout_metadata_free(self_: *mut ShoutMetadataRaw);
    fn shout_metadata_add(
        self_: *mut ShoutMetadataRaw,
        name: *const c_char,
        value: *const c_char,
    ) -> c_int;
    fn shout_set_metadata_utf8(self_: *mut ShoutRaw, metadata: *mut ShoutMetadataRaw) -> c_int;
}

/// One-time libshout initialisation (required before any other call).
static SHOUT_INIT: Once = Once::new();

fn init_libshout() {
    SHOUT_INIT.call_once(|| unsafe { shout_init() });
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("config strings must not contain NUL bytes")
}

fn err_from_ptr(self_: *mut ShoutRaw) -> String {
    if self_.is_null() {
        return "null shout instance".into();
    }
    let msg = unsafe { shout_get_error(self_) };
    if msg.is_null() {
        "unknown libshout error".into()
    } else {
        unsafe { std::ffi::CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Call a `shout_set_*` function on `$self_` with a string value, converting
/// and freeing on failure.
macro_rules! set {
    ($self_:expr, $f:ident, $val:expr) => {{
        let ret = unsafe { $f($self_, cstr($val).as_ptr()) };
        if ret != SHOUTERR_SUCCESS {
            let e = format!("libshout: failed to set {}: {}", stringify!($f), err_from_ptr($self_));
            unsafe { shout_free($self_) };
            return Err(e.into());
        }
    }};
}

/// A live connection to an Icecast server.
pub struct Shout {
    raw: *mut ShoutRaw,
}

// libshout is thread-safe and this connection is owned by a single thread.
unsafe impl Send for Shout {}

impl Shout {
    /// Configure and open a new source connection.
    pub fn connect(config: &OutputConfig, sample_rate: u32, channels: u16) -> Result<Self> {
        init_libshout();

        let raw = unsafe { shout_new() };
        if raw.is_null() {
            return Err("libshout: out of memory allocating connection".into());
        }

        let bitrate_kbps = (config.bitrate / 1000).max(1).to_string();
        let format = match config.format {
            OutputFormat::Mp3 => SHOUT_FORMAT_MP3,
            OutputFormat::Opus => SHOUT_FORMAT_OGG,
        };

        set!(raw, shout_set_host, &config.host);
        if unsafe { shout_set_port(raw, config.port) } != SHOUTERR_SUCCESS {
            let e = format!("libshout: failed to set port: {}", err_from_ptr(raw));
            unsafe { shout_free(raw) };
            return Err(e.into());
        }
        set!(raw, shout_set_user, &config.source_user);
        set!(raw, shout_set_password, &config.source_password);
        set!(raw, shout_set_mount, &config.mount);
        set!(raw, shout_set_agent, "Crabsoup/0.1");

        if unsafe { shout_set_format(raw, format) } != SHOUTERR_SUCCESS {
            let e = format!("libshout: failed to set format: {}", err_from_ptr(raw));
            unsafe { shout_free(raw) };
            return Err(e.into());
        }

        unsafe {
            let _ = shout_set_audio_info(raw, cstr("samplerate").as_ptr(), cstr(&sample_rate.to_string()).as_ptr());
            let _ = shout_set_audio_info(raw, cstr("channels").as_ptr(), cstr(&channels.to_string()).as_ptr());
            let _ = shout_set_audio_info(raw, cstr("bitrate").as_ptr(), cstr(&bitrate_kbps).as_ptr());
            let _ = shout_set_meta(raw, cstr("name").as_ptr(), cstr(&config.name).as_ptr());
            let _ = shout_set_meta(raw, cstr("description").as_ptr(), cstr(&config.description).as_ptr());
            let _ = shout_set_meta(raw, cstr("genre").as_ptr(), cstr(&config.genre).as_ptr());
            let _ = shout_set_public(raw, 0);
        }

        let ret = unsafe { shout_open(raw) };
        if ret != SHOUTERR_SUCCESS {
            let e = format!(
                "libshout: failed to connect to {}:{} ({}): {}",
                config.host,
                config.port,
                ret,
                err_from_ptr(raw)
            );
            unsafe { shout_free(raw) };
            return Err(e.into());
        }

        Ok(Self { raw })
    }

    /// Send encoded bytes. In blocking mode this also paces to real-time when
    /// combined with [`Shout::sync`].
    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let ret = unsafe { shout_send(self.raw, data.as_ptr(), data.len()) };
        if ret != SHOUTERR_SUCCESS {
            Err(format!("libshout send failed: {}", err_from_ptr(self.raw)).into())
        } else {
            Ok(())
        }
    }

    /// Sleep until it is time to send more data (keeps the stream real-time).
    pub fn sync(&mut self) {
        unsafe { shout_sync(self.raw) };
    }

    /// Milliseconds of audio buffered, or how long to wait before sending more.
    pub fn delay_ms(&mut self) -> i32 {
        unsafe { shout_delay(self.raw) }
    }

    /// Update the "now playing" title via the Icecast admin metadata endpoint.
    pub fn update_title(&mut self, title: &str) {
        let meta = unsafe { shout_metadata_new() };
        if meta.is_null() {
            return;
        }
        let added = unsafe { shout_metadata_add(meta, cstr("song").as_ptr(), cstr(title).as_ptr()) };
        if added != SHOUTERR_SUCCESS {
            unsafe { shout_metadata_free(meta) };
            return;
        }
        let ret = unsafe { shout_set_metadata_utf8(self.raw, meta) };
        unsafe { shout_metadata_free(meta) };
        if ret != SHOUTERR_SUCCESS {
            log::warn!("failed to update Icecast metadata: {}", err_from_ptr(self.raw));
        }
    }

    /// Tear down the underlying connection (used before a reconnect).
    pub fn close(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                shout_close(self.raw);
            }
        }
    }
}

impl Drop for Shout {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                shout_close(self.raw);
                shout_free(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libshout_links_and_allocates() {
        // Exercising the FFI verifies that libshout >= 2.4 is installed and
        // that our bindings line up with the installed ABI.
        init_libshout();
        let raw = unsafe { shout_new() };
        assert!(!raw.is_null());
        unsafe { shout_free(raw) };
    }

    #[test]
    fn cstr_rejects_nul() {
        assert!(std::panic::catch_unwind(|| cstr("a\0b")).is_err());
    }
}
