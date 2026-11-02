//! `sol_log_data`, the one `unsafe` call of the program logic (docs/PLAN-improvements.md, I7).
//!
//! Pinocchio 0.11 re-exports the raw syscall (`pinocchio::syscalls::sol_log_data`) with no
//! safe wrapper. The runtime reads an array of `{ addr, len }` byte slices, so this module
//! passes exactly that `#[repr(C)]` layout. Off-chain (host builds: clippy, rustdoc, tests)
//! the call is a no-op.

/// One byte slice as the syscall reads it: a pointer and a length, C layout.
#[cfg(target_os = "solana")]
#[repr(C)]
struct SolBytes {
    addr: *const u8,
    len: u64,
}

/// Logs `data` as one `Program data: <base64>` line.
#[cfg(target_os = "solana")]
#[allow(unsafe_code)]
pub fn data(data: &[u8]) {
    let parts = [SolBytes {
        addr: data.as_ptr(),
        // usize is 64 bits on SBF; a length that did not fit would log nothing.
        len: u64::try_from(data.len()).unwrap_or(0),
    }];
    // SAFETY: `parts` is a live array of one `SolBytes` whose `addr`/`len` describe `data`,
    // a valid, initialized byte slice that outlives the call. `sol_log_data` only reads
    // `data_len` (1) entries of that array and the bytes they point to, and keeps no reference
    // after it returns.
    unsafe {
        pinocchio::syscalls::sol_log_data(parts.as_ptr().cast::<u8>(), 1);
    }
}

/// Host builds: no log syscall exists, so nothing is logged.
#[cfg(not(target_os = "solana"))]
pub fn data(_data: &[u8]) {}
