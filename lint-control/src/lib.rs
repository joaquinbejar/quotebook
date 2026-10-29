//! Failing control for the workspace lint set.
//!
//! Normal builds compile nothing here. `make lint-control` runs `check.sh`, which sets
//! `--cfg lint_control` and runs clippy on this crate. Each probe line below ends with
//! `// expect: <path>`, and the script requires a `disallowed_macros` error on that exact line
//! naming that exact definition path.
//! So removing any single entry from `clippy.toml`, or the deny level from
//! `[workspace.lints]`, fails `make pre-push`.

#![no_std]

#[cfg(lint_control)]
extern crate std;

/// Every banned macro through its `core` path.
#[cfg(lint_control)]
pub fn banned_in_core(x: u8) -> u8 {
    core::assert!(x > 0); // expect: core::assert
    core::assert_eq!(x, 1); // expect: core::assert_eq
    core::assert_ne!(x, 2); // expect: core::assert_ne
    core::debug_assert!(x > 0); // expect: core::debug_assert
    core::debug_assert_eq!(x, 1); // expect: core::debug_assert_eq
    core::debug_assert_ne!(x, 2); // expect: core::debug_assert_ne
    if x == 3 {
        core::panic!("banned"); // expect: core::panic
    }
    if x == 4 {
        core::unreachable!(); // expect: core::unreachable
    }
    x
}

/// The same macros through their `std` paths. `std::panic` is its own definition; the others
/// are re-exports of the `core` macros, so clippy reports the `core` path.
#[cfg(lint_control)]
pub fn banned_in_std(x: u8) -> u8 {
    std::assert!(x > 0); // expect: core::assert
    std::assert_eq!(x, 1); // expect: core::assert_eq
    std::assert_ne!(x, 2); // expect: core::assert_ne
    std::debug_assert!(x > 0); // expect: core::debug_assert
    std::debug_assert_eq!(x, 1); // expect: core::debug_assert_eq
    std::debug_assert_ne!(x, 2); // expect: core::debug_assert_ne
    if x == 3 {
        std::panic!("banned"); // expect: std::panic
    }
    if x == 4 {
        std::unreachable!(); // expect: core::unreachable
    }
    x
}

/// The prelude names, as library code normally writes them.
#[cfg(lint_control)]
pub fn banned_in_prelude(x: u8) -> u8 {
    assert!(x > 0); // expect: core::assert
    debug_assert_eq!(x, 1); // expect: core::debug_assert_eq
    if x == 3 {
        panic!("banned"); // expect: core::panic
    }
    x
}
