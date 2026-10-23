//! Process and CLI exit codes, HTTP security headers, and logging constants.

/// Windows Service / host process exit codes (Win32 error codes with matching SCM descriptions).
pub mod exit_codes {
    /// Service stopped normally (`ERROR_SUCCESS`).
    pub const SUCCESS: i32 = 0;

    /// Invalid configuration or unwritable log directory (`ERROR_INVALID_DATA`: "The data is invalid.").
    pub const INVALID_CONFIGURATION: i32 = 13;

    /// Unexpected runtime or startup failure (`ERROR_EXCEPTION_IN_SERVICE`:
    /// "An exception occurred in the service when handling the control request.").
    pub const UNEXPECTED_FAILURE: i32 = 1064;

    /// Legacy Phase 1 crash loop exit code (`ERROR_PROCESS_ABORTED`:
    /// "The process terminated unexpectedly."). In Phase 2, crash-looping apps transition to `Failed`
    /// while the service continues running.
    pub const CRASH_LOOP: i32 = 1067;

    /// Ctrl+C helper exit code when arguments are invalid.
    pub const HELPER_INVALID_ARGS: i32 = 10;

    /// Ctrl+C helper exit code when `AttachConsole` fails (e.g. target PID already exited).
    pub const HELPER_ATTACH_FAILED: i32 = 11;

    /// Ctrl+C helper exit code when `SetConsoleCtrlHandler` fails.
    pub const HELPER_HANDLER_FAILED: i32 = 12;

    /// Ctrl+C helper exit code when `GenerateConsoleCtrlEvent` fails.
    pub const HELPER_SEND_FAILED: i32 = 14;
}

/// CLI subcommand exit codes (Phase 2 §8).
pub mod cli_exit_codes {
    /// Success (for start/stop/restart: the target state was reached).
    pub const SUCCESS: i32 = 0;

    /// The command ran but failed (e.g. the app ended in `Failed`, or reload validation failed).
    pub const COMMAND_FAILED: i32 = 1;

    /// The service isn't reachable (not running, or wrong `--url`).
    pub const SERVICE_UNREACHABLE: i32 = 2;

    /// Unknown application name.
    pub const UNKNOWN_APPLICATION: i32 = 3;
}

/// HTTP security headers required on Control API requests (Phase 2 §7.3).
pub mod headers {
    /// Required anti-CSRF header on all mutating (`POST`) requests.
    pub const WRAPPER_REQUEST: &str = "x-wrapper-request";

    /// Expected value for `X-Wrapper-Request`.
    pub const WRAPPER_REQUEST_VALUE: &str = "1";

    /// Optional header identifying the caller surface (`dashboard`, `cli`, or `api`).
    pub const WRAPPER_SOURCE: &str = "x-wrapper-source";

    /// Optional header carrying the OS username from the CLI.
    pub const WRAPPER_USER: &str = "x-wrapper-user";
}

/// Log source context identifiers.
pub mod log_sources {
    pub const WRAPPER: &str = "Node.Application.Service.Wrapper";
    pub const CHILD_PROCESS: &str = "Node.Application.Service.Wrapper.ChildProcess";
}

/// Windows Service registration name.
pub const WINDOWS_SERVICE_NAME: &str = "NodeApplicationServiceWrapper";
