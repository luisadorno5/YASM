# Node.Application.Service.Wrapper: Multi-Application Plan (Phase 2)

**Status:** Draft, nothing built yet
**Branch:** `feature/multiple-child-processes` (wrapper repo)
**Builds on:** phase 1 ([implementation plan](node-application-service-wrapper-implementation-plan.md)), which is committed on `main`

## 1. Goal

Phase 1 runs **one wrapper service per application**. Phase 2 changes that to **one wrapper service that runs several applications**. Each application can be started, stopped and restarted on its own from:

- a **web dashboard** served by the wrapper itself;
- a **CLI**, which is the same exe with commands such as `list`, `start` and `stop`.

Everything phase 1 does for one application keeps working for each of the applications:

- its own child process and Job Object;
- graceful Ctrl+C stop;
- restart policy;
- health check;
- port preflight;
- crash context in the logs.

## 2. Decisions

| Topic | Decision | Source |
| --- | --- | --- |
| Hosting model | **One service, many applications.** Replaces one-install-per-app. | Pivot request |
| UI | **Embedded web dashboard.** The service serves a static page plus a REST API (Kestrel). Nothing extra to install. | Decided |
| Access | **Localhost, no authentication.** Anyone logged on to the server can control the apps. Mitigations in §7.3. | Decided |
| Manual stop/start | **Persisted.** An app stopped from the UI or CLI stays stopped after a service or server restart, until someone starts it again. | Decided |
| Application list | **Defined in `appsettings.json`.** A *reload* action (UI or CLI) applies added, removed and changed apps without touching the others. | Decided |
| CLI transport | **The same REST API as the UI.** One control surface, so UI and CLI can't drift apart. | Proposed |
| A crash-looping app | **Goes to `Failed`. The service and the other apps keep running.** Restart policy exit 1067 is no longer a service exit. | Proposed |
| Logging | One wrapper log, with each entry tagged with the app name, plus **one output log per app**. File only, as in phase 1. | Proposed |
| Publish | Framework-dependent, as in phase 1. The host becomes `Microsoft.NET.Sdk.Web`, so servers need the **ASP.NET Core Runtime 10**, not only the .NET Runtime. | Consequence |

## 3. What changes from phase 1

| Area | Phase 1 | Phase 2 |
| --- | --- | --- |
| Configuration | One `ServiceWrapper` section | `Applications` (one entry per app) plus `ApplicationDefaults` |
| Supervisor | `ProcessSupervisor : BackgroundService` (one) | `ApplicationSupervisor` per app, owned by `ApplicationManager` |
| Crash loop | Service stops with exit code 1067 | The app goes to `Failed`; the service keeps running |
| Control | Start/stop the Windows service | Start/stop/restart per app via the dashboard, CLI or API |
| Logs | `<AppName>-wrapper-*.log`, `<AppName>-app-*.log` | `wrapper-*.log` (entries tagged with the app name), `<AppName>-app-*.log` per app |
| Deploy layout | `app\` + `wrapper\` per app | One `wrapper\` plus `apps\<name>\` per application |
| Install | One service per app | One service in total |

These carry over unchanged: `ChildProcess`, `JobObject`, the Ctrl+C helper, `CtrlCRequester`, `RestartPolicy`, `HealthEndpointChecker`, `PathResolver`, `AnsiText`, `OutputLineBuffer`, startup validation and `StartupFailureService`.

The single-app configuration format is dropped. Nothing is in production yet, so no compatibility layer is needed.

## 4. Configuration

### 4.1 Shape

```jsonc
{
  // Shared by every application unless the application overrides it
  "ApplicationDefaults": {
    "ExecutablePath": "%ProgramFiles%\\nodejs\\node.exe",
    "Arguments": "server.js",
    "ShutdownTimeout": "0.00:00:10",
    "VersionArguments": "--version",
    "AutoStart": true,
    "EnvironmentVariables": { "NODE_ENV": "production", "HOSTNAME": "0.0.0.0" },
    "RestartPolicy": { "InitialDelay": "0.00:00:02", "MaxDelay": "0.00:01:00", "MaxRestarts": 5, "RestartWindow": "0.00:10:00" },
    "HealthCheck": { "Enabled": false, "StartupTimeout": "0.00:01:00", "RetryInterval": "0.00:00:02", "RequestTimeout": "0.00:00:05" }
  },

  // One entry per application. The key is the application name (used in the UI, CLI, URLs and log file names).
  "Applications": {
    "storefront": {
      "WorkingDirectory": "..\\apps\\storefront",
      "EnvironmentVariables": { "PORT": "3000" },
      "PreflightPort": 3000,
      "HealthCheck": { "Enabled": true, "Url": "http://127.0.0.1:3000/api/health" }
    },
    "admin": {
      "WorkingDirectory": "..\\apps\\admin",
      "EnvironmentVariables": { "PORT": "3001" },
      "PreflightPort": 3001,
      "AutoStart": false
    }
  },

  "ControlApi": {
    "Url": "http://127.0.0.1:5080"
  },

  "ENABLE_CONSOLE_LOGGING": false,
  "SerilogOptions": { "LogDirectory": "C:\\Logs\\service-wrapper", "MinimumLevel": "Information", "RetainedFileCountLimit": 31, "FileSizeLimitBytes": 104857600, "CrashContextLines": 50 },

  // Where manual start/stop decisions are remembered (relative to the wrapper folder)
  "StateFile": "state.json"
}
```

### 4.2 Rules

- **Applications are keyed by name.** That makes names unique by construction and gives them stable ids for reload. Names follow the phase 1 `AppName` rule (`^[A-Za-z0-9._-]+$`) and are case-insensitive.
- **Effective settings = `ApplicationDefaults` overlaid with the application's own section.** The configuration binder binds the defaults first, then binds the app section onto the same object. As a result:
  - a value an app sets replaces the default;
  - `EnvironmentVariables` are merged key by key (an app's `PORT` is added to the default `NODE_ENV`).
- **`AutoStart`** (default `true`) decides whether an app is started when the service starts, *unless* a manual decision is stored (§5.3).
- **Validation**, all reported in one line with exit code 13, as in phase 1. It runs every phase 1 rule per application, prefixed with the app name, plus:
  - at least one application is defined;
  - no two applications share a `PreflightPort` or an `EnvironmentVariables:PORT`;
  - no application uses the Control API's port;
  - `ControlApi:Url` must be an `http` URL on a **loopback address** (`127.0.0.1`, `::1` or `localhost`). With no authentication, binding to the network must be impossible by configuration.
- The **service-level** settings (`ControlApi`, `SerilogOptions`, `ENABLE_CONSOLE_LOGGING`, `StateFile`) only take effect at service start. A reload that changes them reports "requires a service restart".

## 5. Application lifecycle

### 5.1 States

| State | Meaning |
| --- | --- |
| `Stopped` | Not running, and the wrapper won't start it on its own |
| `Starting` | Process launched; waiting for the health check (or it is disabled) |
| `Running` | Up, and healthy when the health check is enabled |
| `Restarting` | Exited unexpectedly; waiting out the backoff delay before the next attempt |
| `Stopping` | Ctrl+C sent; waiting up to `ShutdownTimeout`, then the tree is killed |
| `Failed` | Gave up after `MaxRestarts` exits in `RestartWindow`. Stays down until someone starts it. |

Each application also exposes:

- process id;
- start time and uptime;
- restart count;
- last exit code and time;
- last error (e.g. "Health check failed | HTTP 404");
- desired state: `Running` or `Stopped`, and whether that comes from `AutoStart` or from a manual action.

### 5.2 Commands

Commands for one app are **serialized**: a start while a stop is in progress waits for the stop to finish. Commands for different apps run independently.

| Command | From `Stopped` / `Failed` | From `Starting` / `Running` / `Restarting` | Persists |
| --- | --- | --- | --- |
| **start** | Resets the restart policy and starts | No-op ("already running") | Desired = Running |
| **stop** | No-op ("already stopped") | Graceful stop (Ctrl+C, then kill after `ShutdownTimeout`) | Desired = Stopped |
| **restart** | Same as start | Stop, then start; the restart policy is reset | Desired = Running |

The API returns as soon as the command is accepted, with the app's current state. Starting can take up to `HealthCheck:StartupTimeout`. The UI polls; the CLI waits for a final state unless `--no-wait` is given.

### 5.3 Persistence of manual decisions (`state.json`)

```json
{ "admin": { "DesiredState": "Running", "ChangedAt": "2026-10-21T15:04:05Z", "ChangedBy": "cli" } }
```

- **Only manual decisions are stored.** An app with no entry follows `AutoStart`.
- **When the service starts,** an app runs if its stored desired state is `Running`, or if it has no entry and `AutoStart` is true.
- **`Failed` doesn't change the desired state.** After a service or server restart, a failed app gets a fresh attempt.
- **Entries for apps that no longer exist are removed on reload.**
- **The file is written atomically** (temp file, then replace), so a crash mid-write can't corrupt it.
- **If the file is corrupt,** it's logged and ignored: every app falls back to `AutoStart`. It never blocks startup.

### 5.4 Reload (`POST /api/configuration/reload`, `reload` command)

1. Re-read `appsettings.json` and validate it completely. **If anything is invalid, nothing changes**, and the errors are returned to the UI or CLI. The running apps keep their current settings.
2. Compare by application name:
   - **Added:** create its supervisor; start it if its desired state is Running.
   - **Removed:** stop it gracefully and drop it; remove its `state.json` entry.
   - **Changed (any effective setting differs):** if it's running, restart it with the new settings; if not, just store the new settings.
   - **Unchanged:** not touched.
3. Return a summary such as `added: admin; removed: legacy; restarted: storefront; unchanged: 2`, plus any service-level settings that need a service restart.

## 6. Architecture

```
Program.cs
 ├─ --send-ctrl-c <pid>             Ctrl+C helper (phase 1, unchanged)
 ├─ list|status|start|stop|...      CLI → HTTP calls to the Control API → exit
 └─ (no command)                    WebApplication host (Windows Service or console)
      ├─ ApplicationManager         (IHostedService) owns the supervisors, applies commands and reloads
      │    ├─ ApplicationSupervisor ×N   per-app loop: start → health → watch → backoff/restart
      │    │     └─ ChildProcess          node.exe + Job Object (phase 1)
      │    └─ DesiredStateStore     state.json
      ├─ ApplicationsConfigurationLoader   defaults + overrides, validation, diff for reload
      └─ Control API + dashboard    Kestrel on ControlApi:Url (loopback only)
```

- **`ApplicationSupervisor`** is phase 1's `ProcessSupervisor` turned into a plain class with `StartAsync`, `StopAsync` and `Snapshot()`. It's no longer a hosted service, and it gets its own options, restart policy and health checker. Its exits never stop the host.
- **`ApplicationManager`** starts the apps **in parallel** when the service starts. When the service stops, it stops them in parallel. `HostOptions.ShutdownTimeout` is the largest app `ShutdownTimeout` plus a margin.
- The **wrapper exits** only for service-level reasons:
  - `13`: invalid configuration at service start;
  - `1064`: an unexpected failure, including the Control API failing to bind its port.

  Windows recovery then restarts the service, and every app comes back according to its desired state.

## 7. Control API and dashboard

### 7.1 REST API (`ControlApi:Url`, default `http://127.0.0.1:5080`)

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/applications` | All apps with state, PID, uptime, restarts, last exit, desired state |
| GET | `/api/applications/{name}` | One app |
| POST | `/api/applications/{name}/start` | Start (202 with current state) |
| POST | `/api/applications/{name}/stop` | Stop (202) |
| POST | `/api/applications/{name}/restart` | Restart (202) |
| GET | `/api/applications/{name}/output?lines=200` | Recent stdout/stderr lines from memory (per-app ring buffer, `OutputBufferLines`, default 500) |
| POST | `/api/configuration/reload` | Reload `appsettings.json` (200 with summary, or 400 with validation errors) |
| GET | `/api/health` | Service health for monitoring: 200 when every app whose desired state is Running is Running, otherwise 503 with details |

Errors use RFC 7807 problem details (`404` unknown app, `409` command conflict, `400` invalid configuration).

### 7.2 Dashboard (`/`)

- One static page (HTML, CSS and plain JavaScript), **embedded in the exe**. There's no frontend build step, and no files can go missing on deploy.
- A table of applications showing:
  - state badge;
  - PID;
  - uptime;
  - restart count;
  - last exit code and time;
  - last error;
  - desired state (and whether it's manual).
- **Start / Stop / Restart** buttons per row, disabled when they wouldn't do anything. Stop asks for confirmation.
- An **output panel** per app showing the last lines, refreshed while open.
- A **Reload configuration** button that shows the summary or the validation errors.
- Auto-refresh every 2 s by polling `GET /api/applications`. Server-sent events can come later if needed.

### 7.3 Security (localhost, no auth)

No authentication was chosen, so anyone logged on to the server can start and stop apps. These guards keep that from becoming a remote or browser-based hole:

1. **Loopback only.** Validation rejects any non-loopback `ControlApi:Url`, so the API can't be exposed by editing configuration.
2. **Host header check.** Requests whose `Host` isn't `127.0.0.1`, `localhost` or `[::1]` (with the configured port) are rejected. This blocks DNS-rebinding attacks from web pages.
3. **Anti-CSRF header.** Every `POST` must carry `X-Wrapper-Request: 1`. The dashboard and CLI send it. A web page in the server's browser can't add a custom header to a cross-site request without a CORS preflight, and CORS isn't enabled.
4. **Every command is logged**, with the source (`dashboard`, `cli` or `api`) and the Windows user where known (`Environment.UserName` for the CLI).

## 8. CLI

The same executable. Any first argument other than `--send-ctrl-c` that matches a command runs the CLI. It finds the API from `ControlApi:Url` in the `appsettings.json` next to the exe; `--url` overrides that.

```powershell
Node.Application.Service.Wrapper.exe list
Node.Application.Service.Wrapper.exe status storefront
Node.Application.Service.Wrapper.exe start admin            # waits for Running (or Failed) unless --no-wait
Node.Application.Service.Wrapper.exe stop storefront
Node.Application.Service.Wrapper.exe restart storefront
Node.Application.Service.Wrapper.exe logs storefront --lines 100
Node.Application.Service.Wrapper.exe reload
```

- A `wrapper.cmd` next to the exe forwards its arguments, so operators can type `wrapper list`.
- `list` prints a table with the same columns as the dashboard.
- Parsing and help use `System.CommandLine` 2.0 (stable). No other new dependency.

**CLI exit codes:**

| Code | Meaning |
| --- | --- |
| 0 | Success (for start/stop/restart: the target state was reached) |
| 1 | The command ran but failed (e.g. the app ended in `Failed`, or reload validation failed) |
| 2 | The service isn't reachable (not running, or wrong `--url`) |
| 3 | Unknown application |

## 9. Logging

| File | Contents |
| --- | --- |
| `<LogDirectory>\wrapper-YYYYMMDD.log` | Service and per-app lifecycle events (compact JSON). Every app event carries an `AppName` property, and the message starts with the app name. |
| `<LogDirectory>\<AppName>-app-YYYYMMDD.log` | That app's stdout/stderr, plain text (one file per app, through `Serilog.Sinks.Map`) |
| Console (`ENABLE_CONSOLE_LOGGING`) | Everything, each line prefixed `[wrapper]` or `[<AppName>]` |

Control actions (start/stop/restart/reload) are logged with their source and user.

## 10. Packaging and installation

```
C:\Program Files\Itdg\service-wrapper\
  wrapper\            Node.Application.Service.Wrapper.exe, appsettings.json, wrapper.cmd, state.json (created at runtime)
  scripts\            Install-Service.ps1 / Uninstall-Service.ps1 (unchanged: still one service)
  apps\
    storefront\       Next.js standalone output
    admin\
```

- `build.ps1` builds the wrapper into `Dist\service-wrapper\wrapper` and each Next.js app into `Dist\service-wrapper\apps\<name>`.
- For testing, the dummy `nextjs-app` is deployed twice under two names and ports, plus a deliberately crashing app.
- **Server prerequisite change:** the **ASP.NET Core Runtime 10** (any 10.x, matching the CPU architecture). On ARM64, that's the ARM64 ASP.NET Core Runtime.

## 11. Implementation steps

Each step ends in a buildable, tested state.

1. **Configuration.**
   - Add the `ApplicationDefaults` + `Applications` model.
   - Implement the overlay binding and per-app plus cross-app validation (unique ports, loopback Control API).
   - Add `ControlApi` options.
2. **`ApplicationSupervisor`.**
   - Refactor `ProcessSupervisor` into a per-app class with an explicit state machine, `Snapshot()`, and serialized `StartAsync`/`StopAsync`/`RestartAsync`.
   - A crash loop goes to `Failed` instead of stopping the host.
3. **`ApplicationManager` + `DesiredStateStore`.**
   - Parallel start and stop.
   - Desired-state rules (§5.3), atomic `state.json`.
   - Reload with diff (§5.4).
4. **Logging.** Tag every entry with `AppName`, write one output log per app, use the `[AppName]` console prefix.
5. **Control API.**
   - Move to `WebApplication` on `Microsoft.NET.Sdk.Web`.
   - Add the endpoints with problem details, the Host header check, the anti-CSRF header and command logging.
6. **Dashboard.** Embedded static page: table, actions, output panel, reload.
7. **CLI.** Commands, `wrapper.cmd`, exit codes.
8. **Packaging.**
   - `build.ps1` multi-app layout.
   - Rewrite the README and the configuration reference.
   - Update the install scripts' validation (the layout check).

## 12. Testing

- **Unit:**
  - configuration overlay (defaults vs overrides, `EnvironmentVariables` merge);
  - cross-app validation (duplicate ports, non-loopback API URL);
  - reload diff (added, removed, changed, unchanged);
  - desired-state rules;
  - `state.json` (atomic write, corrupt file);
  - state-machine transitions and command no-ops;
  - CLI argument parsing and exit codes.
- **Integration (node scripts, as in phase 1):**
  - two apps run side by side, and stopping one leaves the other untouched;
  - a crash-looping app goes to `Failed` while the other stays `Running`;
  - restart resets the policy;
  - reload adds, removes and restarts the right apps;
  - **API** through `WebApplicationFactory`: every endpoint, the 404/400/409 responses, a POST without `X-Wrapper-Request` rejected, a bad Host header rejected.
- **Acceptance on a real service** (as in phase 1 §14), with two copies of the Next.js app plus a crashing app:
  - start and stop each one from the dashboard and from the CLI;
  - stop one, restart the service, and confirm it stays stopped while the others come back;
  - the crash loop affects only its own app;
  - reload: add, remove and change ports;
  - kill the wrapper: every node dies, and recovery brings back the right set;
  - `Stop-Service` stops every app gracefully, in parallel.

## 13. Risks

| Risk | Mitigation |
| --- | --- |
| No authentication: any logged-on user can stop production apps | Accepted (decided). Loopback-only, Host check, anti-CSRF header, and every action logged with its source. Revisit if non-admins can log on to the servers. |
| Servers lack the ASP.NET Core Runtime | Documented as a prerequisite. The install script checks for it and fails with a clear message. |
| One misbehaving app affects others (CPU, memory, ports) | Separate processes and Job Objects; port uniqueness validated. Per-app memory limits are a later phase. |
| A slow app delays service start or stop | Apps start and stop in parallel. Windows sees the service as *Running* immediately, and each app's health wait is independent. |
| Reload applied while commands are running | Reload takes each affected app's command lock, so it runs between commands, never during one. |

## 14. Out of scope (later)

- Authentication or network access for the dashboard.
- Editing applications from the UI.
- Start order and dependencies between apps.
- Periodic health probes after startup (phase 1 backlog), per-app memory limits, scheduled restarts.
- Server-sent events for the dashboard.
