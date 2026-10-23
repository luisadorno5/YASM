//! Unix/macOS implementation of process group isolation, orphan cleanup, and `--send-ctrl-c <pid>` helper.

use crate::constants::exit_codes;
use tokio::process::Command;

/// Configure a `tokio::process::Command` before spawning on Unix:
/// Places the child in its own process group (`pgid == pid`) so signals (`SIGINT`, `SIGKILL`)
/// can be delivered cleanly to the entire process tree without affecting the wrapper itself.
pub fn configure_child_command(cmd: &mut Command) {
    cmd.process_group(0);
}

/// RAII process-group guard on Unix mirroring Windows `JobObject` (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
/// Dropping this guard terminates the child process group so no orphan or grandchild processes survive.
pub struct ProcessJobGuard {
    pid: u32,
}

impl ProcessJobGuard {
    pub fn attach(pid: u32) -> Result<Self, String> {
        if pid == 0 {
            return Err("Invalid PID 0".to_string());
        }
        Ok(Self { pid })
    }

    /// Kill the entire process group (`-pid`) and direct process (`pid`).
    pub fn kill_tree(&self) {
        let pgid = -(self.pid as i32);
        // SAFETY: Sending SIGKILL to the child's dedicated process group.
        unsafe {
            let _ = libc::kill(pgid, libc::SIGKILL);
            let _ = libc::kill(self.pid as i32, libc::SIGKILL);
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for ProcessJobGuard {
    fn drop(&mut self) {
        self.kill_tree();
    }
}

/// Execute the `--send-ctrl-c <childPid>` helper mode on Unix by sending `SIGINT`
/// to the child's process group (and direct PID).
pub fn execute_send_ctrl_c_helper(pid: u32) -> i32 {
    if pid == 0 {
        return exit_codes::HELPER_INVALID_ARGS;
    }

    let pgid = -(pid as i32);
    // SAFETY: Standard POSIX kill() call.
    unsafe {
        // Check if process or process group exists and send SIGINT
        let res_pg = libc::kill(pgid, libc::SIGINT);
        let res_pid = libc::kill(pid as i32, libc::SIGINT);
        if res_pg != 0 && res_pid != 0 {
            return exit_codes::HELPER_ATTACH_FAILED;
        }
    }

    exit_codes::SUCCESS
}
