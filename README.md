# Node.Application.Service.Wrapper (Rust Implementation)

A security-hardened, low-footprint Windows Service and multi-application process supervisor written in **Rust**. It implements the complete Phase 1 and Phase 2 specifications ([Implementation Plan](node-application-service-wrapper-implementation-plan.md), [Configuration Reference](node-application-service-wrapper-configuration.md), and [Multi-Application Plan](node-application-service-wrapper-multi-app-plan.md)) as a single native executable with **zero external runtime dependencies** (no .NET Runtime or ASP.NET Core Runtime required on servers).

---

## Key Pillars

1. **Low Profile Resource Footprint**
   - Compiles to a single stripped native binary (~3.4 MB release build with `opt-level = "s"`, `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`).
   - Zero garbage collector overhead; uses bounded ring buffers (`OutputLineBuffer`) for crash context (`CrashContextLines`) and live output (`OutputBufferLines`).
   - Direct Win32 FFI via `windows-sys` and `windows-service` on Windows, with Unix process-group equivalents on macOS/Linux for local development and testing.

2. **Security by Design**
   - **Loopback-Only Control API**: Startup and reload validation reject any non-loopback `ControlApi:Url` (`127.0.0.1`, `::1`, or `localhost`).
   - **DNS-Rebinding Protection**: Middleware verifies the `Host` header on every HTTP request and rejects non-loopback hosts with `403 Forbidden` (`application/problem+json`).
   - **Anti-CSRF Enforcement**: All mutating (`POST`) endpoints require `X-Wrapper-Request: 1`. Cross-origin requests are blocked (no CORS headers emitted).
   - **Hardened HTTP Headers & Bounded Payloads**: Emits strict `Content-Security-Policy`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Cache-Control: no-store`, and enforces a 16 KiB body size limit.
   - **Secret Protection**: Environment variable *names* are logged at startup; *values* are never written to logs or exposed via the API.
   - **Audit Logging**: Every control action (`start`, `stop`, `restart`, `reload`) is logged with its source (`dashboard`, `cli`, or `api`) and OS user.

---

## Project Structure

```text
windows-node-service-manager/
├─ Cargo.toml
├─ appsettings.json
├─ appsettings.Development.json
├─ wrapper.cmd
├─ build.ps1
├─ scripts/
│  ├─ Install-Service.ps1
│  └─ Uninstall-Service.ps1
├─ src/
│  ├─ main.rs              # Entrypoint (--send-ctrl-c helper, CLI subcommands, Service/Console host)
│  ├─ lib.rs               # Library root
│  ├─ ansi.rs              # Zero-regex ANSI CSI/OSC escape sequence stripper
│  ├─ api.rs               # Axum Control REST API, RFC 7807 Problem Details, Host & CSRF security middleware
│  ├─ buffer.rs            # Thread-safe bounded ring buffer (OutputLineBuffer)
│  ├─ cli.rs               # CLI client (list, status, start, stop, restart, logs, reload)
│  ├─ config.rs            # ApplicationDefaults + Applications overlay, validation, and reload diffing
│  ├─ constants.rs         # Win32 service exit codes (0, 13, 1064, 1067) and CLI exit codes (0, 1, 2, 3)
│  ├─ dashboard.html       # Embedded zero-dependency single-page web dashboard
│  ├─ duration.rs          # .NET TimeSpan parser and formatter (d.hh:mm:ss)
│  ├─ health.rs            # Startup HTTP health endpoint poller (HealthEndpointChecker)
│  ├─ logging.rs           # Serilog-compatible Compact JSON wrapper log + plain-text per-app rolling logs
│  ├─ manager.rs           # Multi-application manager, parallel start/stop, and live reload coordinator
│  ├─ path_resolver.rs     # %ENV_VAR% expansion, relative path resolution, and PATH/PATHEXT lookup
│  ├─ port_checker.rs      # TCP PreflightPort checker
│  ├─ restart_policy.rs    # Sliding-window exponential backoff calculator
│  ├─ service.rs           # Windows SCM dispatcher (StartupFailureService) and console host runner
│  ├─ state_store.rs       # Atomic state.json persistence for manual start/stop decisions
│  ├─ supervisor.rs        # Per-application state machine and child process supervision loop
│  └─ platform/
│     ├─ mod.rs
│     ├─ windows.rs        # Win32 Job Objects (KILL_ON_JOB_CLOSE) and AttachConsole Ctrl+C sender
│     └─ unix.rs           # POSIX process groups and SIGINT/SIGKILL tree cleanup
└─ tests/
   └─ integration_tests.rs # End-to-end integration test suite
```

---

## Building and Testing

```bash
# Run all unit and integration tests
cargo test

# Build optimized release binary
cargo build --release
```

On Windows PowerShell, package the full multi-app deployment directory (`Dist\service-wrapper`):

```powershell
.\build.ps1
```

---

## CLI Usage

The same executable provides both the service host and the operator CLI (`wrapper.cmd` forwards arguments to `Node.Application.Service.Wrapper.exe`):

```powershell
wrapper list
wrapper status storefront
wrapper start admin            # Waits for Running (or Failed) unless --no-wait is passed
wrapper stop storefront
wrapper restart storefront
wrapper logs storefront --lines 100
wrapper reload
```

---

## Windows Service Installation

Using the included PowerShell scripts:

```powershell
.\scripts\Install-Service.ps1 -InstallRoot "C:\Program Files\Itdg\service-wrapper"
```

Or manually via `sc.exe`:

```powershell
sc.exe create NodeApplicationServiceWrapper binPath= "\"C:\Program Files\Itdg\service-wrapper\wrapper\Node.Application.Service.Wrapper.exe\"" start= delayed-auto
sc.exe failure NodeApplicationServiceWrapper reset= 86400 actions= restart/5000/restart/5000/restart/30000
sc.exe failureflag NodeApplicationServiceWrapper 1
```
