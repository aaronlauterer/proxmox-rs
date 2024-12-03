//! Rust language related helpers.
//!
//! This provides some macros for features which are not yet available in the language, or
//! sometimes also types from nightly `std` which are simple enough to do just haven't been
//! bikeshedded and stabilized in the standard library yet.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod constnamedbitmap;

pub mod error;
pub mod ops;

/// Macro to write error-handling blocks (like perl eval {})
///
/// #### Example:
/// ```
/// # use proxmox_lang::try_block;
/// # macro_rules! format_err {
/// #     ($($msg:tt)+) => { format!($($msg)+) }
/// # }
/// # macro_rules! bail {
/// #     ($($msg:tt)+) => { return Err(format_err!($($msg)+)); }
/// # }
/// # let some_condition = false;
/// let result = try_block!({
///     if (some_condition) {
///         bail!("some error");
///     }
///     Ok(())
/// })
/// .map_err(|e| format_err!("my try block returned an error - {}", e));
/// ```
#[macro_export]
macro_rules! try_block {
    { $($token:tt)* } => {{ (|| -> Result<_,_> { $($token)* })() }}
}

/// Statically assert the size of a type at compile time.
///
/// This should compile:
/// ```
/// # use proxmox_lang::static_assert_size;
/// #[repr(C)]
/// struct Stuff {
///     value: [u8; 32]
/// }
/// static_assert_size!(Stuff, 32);
/// ```
///
/// This should fail to compile:
/// ```compile_fail
/// # use proxmox_lang::static_assert_size;
/// #[repr(C)]
/// struct Stuff {
///     value: [u8; 32]
/// }
/// static_assert_size!(Stuff, 128);
/// ```
#[macro_export]
macro_rules! static_assert_size {
    ($ty:ty, $size:expr) => {
        const _: fn() -> () = || {
            let _ = ::std::mem::transmute::<[u8; $size], $ty>;
        };
    };
}

/// Evaluates to the offset (in bytes) of a given member within a struct
///
/// ```
/// # use proxmox_lang::offsetof;
///
/// #[repr(C)]
/// struct Stuff {
///     first: u32,
///     second: u32,
/// }
///
/// assert_eq!(offsetof!(Stuff, second), 4);
///
/// ```
#[deprecated = "use std::mem::offset_of! instead"]
#[macro_export]
macro_rules! offsetof {
    ($ty:ty, $field:ident) => {
        unsafe { &(*(std::ptr::null::<$ty>())).$field as *const _ as usize }
    };
}

/// Shortcut for generating an `&'static CStr`.
///
/// This takes a *string* (*not* a *byte-string*), appends a terminating zero, and calls
/// `CStr::from_bytes_with_nul_unchecked`.
///
/// Shortcut for:
/// ```no_run
/// let bytes = concat!("THE TEXT", "\0");
/// unsafe { ::std::ffi::CStr::from_bytes_with_nul_unchecked(bytes.as_bytes()) }
/// # ;
/// ```
#[deprecated = "use c\"literals\" instead"]
#[macro_export]
macro_rules! c_str {
    ($data:expr) => {{
        unsafe { ::std::ffi::CStr::from_bytes_with_nul_unchecked(concat!($data, "\0").as_bytes()) }
    }};
}
