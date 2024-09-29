extern crate libc;
use std::io::Write;

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

pub fn localtime_r(seconds: i64, tm: &mut libc::tm) {
    let t = seconds as libc::time_t;
    unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::localtime_r(&t, tm);
        }
        #[cfg(not(target_os = "linux"))]
        {
            libc::localtime_s(tm, &t);
        }
    }
}
pub fn gmtime_r(seconds: i64, tm: &mut libc::tm) {
    let t = seconds as libc::time_t;
    unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::gmtime_r(&t, tm);
        }
        #[cfg(not(target_os = "linux"))]
        {
            libc::gmtime_s(tm, &t);
        }
    }
}

pub fn format_time(
    buffer: &mut [u8],
    nownanos: i64,
    subsecond_digits: u32, // only be 0, 3, 6, 9
    gmt_time: bool,
) -> &str {
    debug_assert!(buffer.len() as u32 > 17 + subsecond_digits + 1);
    let (seconds, nanos) = (nownanos / 1000000000, nownanos % 1000000000);
    let mut tm: libc::tm = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };

    if gmt_time {
        gmtime_r(seconds, &mut tm);
    } else {
        localtime_r(seconds, &mut tm);
    }
    write!(
        &mut buffer[..],
        "{:04}{:02}{:02}-{:02}:{:02}:{:02}",
        (tm.tm_year + 1900),
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour + 1,
        tm.tm_min,
        tm.tm_sec
    )
    .unwrap();
    let mut n = 17usize;
    if subsecond_digits > 0 && subsecond_digits < 10 {
        if subsecond_digits == 0 {
            write!(&mut buffer[n..], ".{:03}", nanos / 1000000).unwrap();
        } else if subsecond_digits == 3 {
            write!(&mut buffer[n..], ".{:06}", nanos / 1000).unwrap();
        } else {
            write!(&mut buffer[n..], ".{:09}", nanos).unwrap();
        }
        n += ((subsecond_digits / 3) * 3 + 1) as usize;
    }
    std::str::from_utf8(&buffer[..n]).unwrap()
}

pub fn now_nanos() -> i64 {
    return std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
}

// Useful when windows timespec_get has only low resolution.
pub fn cpu_now_nanos() -> i64 {
    let epoch: std::time::Instant =
        unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
    return std::time::Instant::now()
        .duration_since(epoch)
        .as_nanos() as i64;
}

#[macro_export]
macro_rules! logmsg {
    ($( $args:expr ),*) => {
        let mut buf = [0u8; 40];
        print!("[{}] ", crate::utils::format_time(&mut buf, crate::utils::now_nanos(), 6, false));
        println!( $( $args ),* );
    }
}

#[macro_export]
/// log only in debug mode.
#[cfg(debug_assertions)]
macro_rules! dbglog {
    ($( $args:expr ),*) => {
        let mut buf = [0u8; 40];
        print!("[{}] [DBG] ", crate::utils::format_time(&mut buf, crate::utils::now_nanos(), 6, false));
        println!( $( $args ),* );
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

    #[test]
    pub fn test_reactio() {
        let mut _buf = [0u8; 32];
        dbglog!("test dbglog.");
        // logmsg!("any msg");
    }
}
