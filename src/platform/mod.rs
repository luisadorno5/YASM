//! Platform abstraction for Win32 Job Objects, Console Ctrl+C helper delivery, and Unix process groups.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
