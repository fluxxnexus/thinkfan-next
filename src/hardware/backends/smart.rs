//! On-demand libatasmart binding; disk handles are scoped to each read.
use anyhow::{Context, Result, ensure};
use libloading::Library;
use std::{
    ffi::{CString, c_char, c_int, c_uint, c_void},
    path::Path,
    ptr,
};
type DiskOpen = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int;
type DiskFree = unsafe extern "C" fn(*mut c_void);
type DiskSleep = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> c_int;
type DiskRead = unsafe extern "C" fn(*mut c_void) -> c_int;
type DiskTemp = unsafe extern "C" fn(*mut c_void, *mut u64) -> c_int;
struct SmartApi {
    _library: Library,
    open: DiskOpen,
    free: DiskFree,
    sleep: DiskSleep,
    read: DiskRead,
    temperature: DiskTemp,
}
impl SmartApi {
    fn load() -> Result<Self> {
        let _measure = crate::metrics::Measure::start(crate::metrics::Operation::SmartLoad);
        // SAFETY: signatures match libatasmart's ABI. The owned library outlives
        // every invocation of these private copied function pointers.
        unsafe {
            let library =
                Library::new("libatasmart.so.4").context("atasmart needs libatasmart.so.4")?;
            Ok(Self {
                open: *library.get(b"sk_disk_open\0")?,
                free: *library.get(b"sk_disk_free\0")?,
                sleep: *library.get(b"sk_disk_check_sleep_mode\0")?,
                read: *library.get(b"sk_disk_smart_read_data\0")?,
                temperature: *library.get(b"sk_disk_smart_get_temperature\0")?,
                _library: library,
            })
        }
    }
}
#[derive(Default)]
pub struct AtaSmartSensor {
    api: Option<SmartApi>,
}
impl AtaSmartSensor {
    pub fn read(&mut self, path: &Path, no_wakeup: bool) -> Result<Option<f64>> {
        use std::os::unix::ffi::OsStrExt;
        let path = CString::new(path.as_os_str().as_bytes())?;
        if self.api.is_none() {
            self.api = Some(SmartApi::load()?);
        }
        let api = self.api.as_ref().unwrap();
        // Disk handles are deliberately opened per read. Sleep checks happen on
        // each new handle; no stale disk pointer survives device replacement.
        unsafe {
            let mut disk = ptr::null_mut();
            ensure!(
                (api.open)(path.as_ptr(), &mut disk) >= 0 && !disk.is_null(),
                "libatasmart open failed: {}",
                std::io::Error::last_os_error()
            );
            struct Disk(*mut c_void, DiskFree);
            impl Drop for Disk {
                fn drop(&mut self) {
                    unsafe {
                        (self.1)(self.0);
                    }
                }
            }
            let _guard = Disk(disk, api.free);
            if no_wakeup {
                let mut awake = 0;
                ensure!(
                    (api.sleep)(disk, &mut awake) >= 0,
                    "libatasmart sleep check failed"
                );
                if awake == 0 {
                    return Ok(None);
                }
            }
            ensure!((api.read)(disk) >= 0, "libatasmart SMART read failed");
            let mut value = 0;
            ensure!(
                (api.temperature)(disk, &mut value) >= 0,
                "libatasmart temperature read failed"
            );
            Ok(Some(value as f64 / 1000.0 - 273.15))
        }
    }
}
