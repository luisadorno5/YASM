//! Windows implementation: Win32 Job Objects (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`),
//! `CREATE_NO_WINDOW` child configuration, Ctrl+C inheritance reset, and `--send-ctrl-c <pid>` helper.

use crate::constants::exit_codes;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null;
use tokio::process::Command;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, FALSE, HANDLE, TRUE};
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_C_EVENT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, CREATE_NO_WINDOW, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};

/// Configure a `tokio::process::Command` before spawning on Windows:
/// - Resets the parent's "ignore Ctrl+C" flag so the child does not inherit it (Phase 1 §13).
/// - Sets `CREATE_NO_WINDOW` (gives the child its own hidden console for `AttachConsole`,
///   and intentionally omits `CREATE_NEW_PROCESS_GROUP` which would disable Ctrl+C).
pub fn configure_child_command(cmd: &mut Command) {
    // SAFETY: Passing (None, FALSE) to SetConsoleCtrlHandler restores normal Ctrl+C inheritance.
    unsafe {
        let _ = SetConsoleCtrlHandler(None, FALSE);
    }
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// RAII Win32 Job Object handle (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) owning a child process tree.
/// Dropping this guard closes the job handle, causing the Windows kernel to immediately terminate
/// the child process and all of its descendants.
pub struct ProcessJobGuard {
    job_handle: HANDLE,
    pid: u32,
}

// SAFETY: Win32 Job Object HANDLE is thread-safe and owned exclusively by `ProcessJobGuard`.
unsafe impl Send for ProcessJobGuard {}
unsafe impl Sync for ProcessJobGuard {}

impl ProcessJobGuard {
    /// Create a new Win32 Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assign `pid` to it.
    pub fn attach(pid: u32) -> Result<Self, String> {
        unsafe {
            let job = CreateJobObjectW(null(), null());
            if job.is_null() {
                return Err(format!(
                    "CreateJobObjectW failed with Win32 error {}",
                    GetLastError()
                ));
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let set_ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set_ok == FALSE {
                let err = GetLastError();
                let _ = CloseHandle(job);
                return Err(format!(
                    "SetInformationJobObject(KILL_ON_JOB_CLOSE) failed with Win32 error {err}"
                ));
            }

            let proc_handle = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, FALSE, pid);
            if proc_handle.is_null() {
                let err = GetLastError();
                let _ = CloseHandle(job);
                return Err(format!(
                    "OpenProcess(pid={pid}) for Job Object failed with Win32 error {err}"
                ));
            }

            let assign_ok = AssignProcessToJobObject(job, proc_handle);
            let assign_err = GetLastError();
            let _ = CloseHandle(proc_handle);

            if assign_ok == FALSE {
                let _ = CloseHandle(job);
                return Err(format!(
                    "AssignProcessToJobObject(pid={pid}) failed with Win32 error {assign_err}"
                ));
            }

            Ok(Self {
                job_handle: job,
                pid,
            })
        }
    }

    /// Explicitly terminate all processes in the Job Object.
    pub fn kill_tree(&self) {
        if !self.job_handle.is_null() {
            // SAFETY: Valid job handle owned by `self`.
            unsafe {
                let _ = TerminateJobObject(self.job_handle, 1);
            }
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for ProcessJobGuard {
    fn drop(&mut self) {
        if !self.job_handle.is_null() {
            // SAFETY: Valid job handle; closing it with KILL_ON_JOB_CLOSE kills any remaining descendants.
            unsafe {
                let _ = TerminateJobObject(self.job_handle, 1);
                let _ = CloseHandle(self.job_handle);
            }
            self.job_handle = std::ptr::null_mut();
        }
    }
}

/// Execute the `--send-ctrl-c <childPid>` helper mode (Phase 1 §6.4):
/// 1. `FreeConsole()`
/// 2. `AttachConsole(childPid)`
/// 3. `SetConsoleCtrlHandler(None, TRUE)` (ignore the Ctrl+C event in the helper itself)
/// 4. `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)`
pub fn execute_send_ctrl_c_helper(pid: u32) -> i32 {
    if pid == 0 {
        return exit_codes::HELPER_INVALID_ARGS;
    }

    unsafe {
        // Detach from any existing console (succeeds or fails harmlessly if already detached in Session 0)
        let _ = FreeConsole();

        if AttachConsole(pid) == FALSE {
            return exit_codes::HELPER_ATTACH_FAILED;
        }

        if SetConsoleCtrlHandler(None, TRUE) == FALSE {
            let _ = FreeConsole();
            return exit_codes::HELPER_HANDLER_FAILED;
        }

        let sent = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
        let _ = FreeConsole();

        if sent == FALSE {
            return exit_codes::HELPER_SEND_FAILED;
        }
    }

    exit_codes::SUCCESS
}
