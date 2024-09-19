extern crate libc;
pub mod flat_storage;
pub mod poller;

pub mod utils {
    pub fn ref_to_bytes<T>(val: &T) -> &[u8] {
        unsafe {
            return std::slice::from_raw_parts(val as *const T as *const u8, 1);
        }
    }
    pub fn ref_to_bytes_mut<T>(val: &mut T) -> &mut [u8] {
        unsafe {
            return std::slice::from_raw_parts_mut(val as *mut T as *mut u8, 1);
        }
    }
    pub fn bytes_to_ref_mut<T>(buf: &mut [u8]) -> &mut T {
        unsafe {
            let p = buf.as_mut_ptr() as *mut T;
            return &mut *p;
        }
    }
    pub fn bytes_to_ref<T>(buf: &[u8]) -> &T {
        unsafe {
            let p = buf.as_ptr() as *const T;
            return &*p;
        }
    }

    pub fn format_time(
        buffer: &mut [u8],
        nownanos: i64,
        subsecond_digits: u32,
        gmt_time: bool,
    ) -> &str {
        debug_assert!(buffer.len() as u32 > 17 + subsecond_digits + 1);
        const SUBSEC_FMT: [&'static str; 3] = [".%03ld\0", ".%06ld\0", ".%09ld\0"]; // rust string doesn't terminate with 0.
        const SUBSEC_FACTOR: [i64; 3] = [1000000, 1000, 1];

        let (seconds, nanos) = (nownanos / 1000000000, nownanos % 1000000000);
        let mut tm: libc::tm = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };

        let fmt = "%Y%m%d-%T\0";
        let mut n;
        unsafe {
            let buf = buffer.as_mut_ptr() as *mut libc::c_char;
            if gmt_time {
                libc::gmtime_r(&seconds, &mut tm);
            } else {
                libc::localtime_r(&seconds, &mut tm);
            }
            n = libc::strftime(buf, 40, fmt.as_ptr() as *const libc::c_char, &tm);
            if subsecond_digits > 0 && subsecond_digits < 10 {
                // only be 3, 6, 9
                let idx = subsecond_digits / 3 - 1;
                let fmt: &'static str = SUBSEC_FMT[idx as usize];
                let subval: i64 = nanos / SUBSEC_FACTOR[idx as usize];
                n += libc::snprintf(
                    buf.add(n),
                    buffer.len() - n,
                    fmt.as_ptr() as *const libc::c_char,
                    subval,
                ) as usize;
            }
            std::str::from_utf8(&buffer[..n]).unwrap()
        }
    }

    pub fn now_nanos() -> i64 {
        return std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
    }
}

#[macro_export]
macro_rules! logmsg {
    ($( $args:expr ),*) => {
        let mut buf = [0u8; 40];
        print!("[{}] ", utils::format_time(&mut buf, utils::now_nanos(), 6, false));
        println!( $( $args ),* );
    }
}

#[macro_export]
/// log only in debug mode.
#[cfg(debug_assertions)]
macro_rules! dbglog {
    ($( $args:expr ),*) => {
        logmsg!( $( $args ),* );
    }
}
#[allow(unused_macros)]
#[macro_export]
#[cfg(not(debug_assertions))]
macro_rules! dbglog {
    ($( $args:expr ),*) => {
        ()
    };
}

#[cfg(test)]
mod test {
    // use super::*;

    use crate::utils;

    #[test]
    pub fn test_reactio() {
        // let mut buf = [0u8; 32];
        dbglog!("test dbglog.");
        assert_eq!(2, 2);
    }
}
