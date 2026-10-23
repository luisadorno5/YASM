# Node.Application.Service.Wrapper — Implementation Plan

**Status:** Phase 1 (steps 1–8) is implemented. 75 automated tests pass. The real-service acceptance tests pass (§14). §15 records the exit-code decision. §13 has the Ctrl+C spike results.
**Target:** .NET 10 Worker Service running as a Windows Service on Windows Server
**Test app:** `nextjs-app/` (dummy Next.js standalone app in this directory)

## 1. Goal

A generic Windows Service host that starts a long-running executable, keeps it running and logs what it does. The first user is a Next.js standalone app (`node server.js`). Nothing about Node or Next.js is hard-coded. The executable, arguments, working directory and environment all come from `appsettings.json`, so the same wrapper can run any app.

## 2. Decisions

| Topic | Decision |
| --- | --- |
| Installation model | **One wrapper install per app.** Each app gets its own copy of the wrapper and its own `appsettings.json`. |
| Logging | **File logs.** No Windows Event Log, no OpenTelemetry, no App Insights. A console sink is available only when `ENABLE_CONSOLE_LOGGING` is `true`, which is meant for local development. |
| Health endpoint | **Optional, controlled by `HealthCheck:Enabled` and `HealthCheck:Url`.** Apps that don't expose a health endpoint turn it off. |
| Graceful stop | **Send Ctrl+C to the child, wait `ShutdownTimeout`, then kill the whole process tree.** |
| Publish | **Framework-dependent `win-x64`** (company precedent). The server needs the .NET 10 runtime. `appsettings.json` stays editable on the server. |
| Scope of this phase | Process launch, supervision and logging (sections 1–3 of the brainstorm). |
| Project structure | One host project and one test project. No Domain/Application/Infrastructure split: the wrapper is too small to benefit from it. |

## 3. Scope

**In scope (this phase)**

- Launch a configured executable with arguments, working directory and environment variables.
- Validate configuration at startup and fail fast with a clear message.
- Capture the child's stdout/stderr into a log file.
- Restart the child when it exits unexpectedly, with exponential backoff.
- Stop restarting after too many crashes in a time window. Exit with a non-zero code so the Windows Service Control Manager (SCM) recovery actions can take over.
- Kill the child (and its children) if the wrapper dies, using a Windows Job Object.
- Graceful stop: Ctrl+C, then kill the process tree after a timeout.
- Optional health check after every start (`HealthCheck:Enabled`). An app that never answers on `HealthCheck:Url` is killed, and the attempt counts as a failed start.
- Optional port preflight: detect that the app's port is already taken before starting.
- Log the runtime version at startup (for example `node --version`).

**Out of scope (later phases)**

- Periodic HTTP health probe with restart on repeated failure. It will reuse the `HealthCheck` section.
- Memory ceiling, scheduled restarts.
- Wrapper status endpoint.
- Install/uninstall PowerShell scripts. For this phase, `sc.exe` commands are documented in the README instead.
- `azure-pipelines.yml`.

## 4. Repository layout

The layout follows `FirstBank-OnlineSales-ClientAPI`: hyphenated repo folder, all code under `Source\` next to the `.sln`, flat project folders named after their projects, no solution folders.

```
nextjs-net-service-wrapper\
├─ Documentation\                          ← this plan
├─ nextjs-app\                             ← dummy test app (separate project)
└─ Node-Application-Service-Wrapper\       ← its own git repo
   ├─ README.md
   ├─ .gitignore
   └─ Source\
      ├─ global.json
      ├─ Node.Application.Service.Wrapper.sln
      ├─ Node.Application.Service.Wrapper\
      │  ├─ Program.cs
      │  ├─ appsettings.json
      │  ├─ appsettings.Development.json
      │  ├─ Constants\       ExitCodes.cs, LogSources.cs
      │  ├─ Extensions\      ServiceCollectionExtensions.cs, LoggingExtensions.cs,
      │  │                   ExceptionExtensions.cs
      │  ├─ Interop\         NativeMethods.cs, SafeJobHandle.cs, JobObject.cs, ConsoleCtrlSender.cs
      │  ├─ Options\         ServiceWrapperOptions.cs, RestartPolicyOptions.cs,
      │  │                   HealthCheckOptions.cs, SerilogOptions.cs,
      │  │                   ServiceWrapperOptionsValidator.cs
      │  ├─ Services\        ProcessSupervisor.cs, ChildProcess.cs, CtrlCRequester.cs,
      │  │                   CtrlCHelperCommand.cs, RestartPolicy.cs, OutputLineBuffer.cs,
      │  │                   PortChecker.cs, HealthEndpointChecker.cs, RuntimeVersionReader.cs,
      │  │                   ServiceExitState.cs, StartupFailureService.cs
      │  └─ Utilities\       PathResolver.cs, AnsiText.cs
      └─ Node.Application.Service.Wrapper.Tests\
```

Notes:

- Create the solution as a classic `.sln` (`dotnet new sln --format sln`), because the .NET 10 SDK defaults to `.slnx` and every company repo uses `.sln`.
- `Source\global.json` pins the SDK, using the value recommended in `C:\dev\OSS\documentation`:
  `{ "sdk": { "version": "10.0.300", "rollForward": "latestMinor" } }`.
- Every `PackageReference` has an explicit version. `Microsoft.Extensions.*` versions stay aligned with 10.0.x.

### Server deployment layout (one install per app)

```
C:\Apps\nextjs-app\
├─ app\        ← contents of .next\standalone (server.js, .next\, public\, node_modules\)
└─ wrapper\    ← published wrapper + appsettings.json
```

Relative paths in `appsettings.json` resolve against the wrapper's own folder, so `"WorkingDirectory": "..\\app"` works without hard-coding drive letters.

## 5. Configuration

### 5.1 Sample `appsettings.json`

The shipped `appsettings.json` has no comments. Every setting is described in [node-application-service-wrapper-configuration.md](node-application-service-wrapper-configuration.md). The comments below are only for this plan.

```jsonc
{
  "ServiceWrapper": {
    // Label used in log file names and log entries. Not required to match the Windows service name.
    "AppName": "nextjs-app",
    // Full path, %ENV_VAR% path, or a bare name resolved through PATH (e.g. "node").
    "ExecutablePath": "%ProgramFiles%\\nodejs\\node.exe",
    "Arguments": "server.js",
    "WorkingDirectory": "..\\app",
    "EnvironmentVariables": {
      "PORT": "3000",
      "HOSTNAME": "0.0.0.0",
      "NODE_ENV": "production"
    },
    "ShutdownTimeout": "0.00:00:10",
    // Optional: logged once at startup. Empty = skip.
    "VersionArguments": "--version",
    // Optional: fail the start attempt with a clear message if this port is already in use.
    "PreflightPort": 3000
  },
  "HealthCheck": {
    // false = skip all health checking (for apps without a health endpoint). Url is then ignored.
    "Enabled": true,
    "Url": "http://localhost:3000/api/health",
    // How long to wait after each start for the first success status code.
    "StartupTimeout": "0.00:01:00",
    // Delay between attempts while waiting.
    "RetryInterval": "0.00:00:02",
    // Per-request timeout.
    "RequestTimeout": "0.00:00:05"
  },
  "RestartPolicy": {
    "InitialDelay": "0.00:00:02",
    "MaxDelay": "0.00:01:00",
    "MaxRestarts": 5,
    "RestartWindow": "0.00:10:00"
  },
  // Also write logs to the console. Keep false on servers; appsettings.Development.json sets it to true.
  "ENABLE_CONSOLE_LOGGING": false,
  "SerilogOptions": {
    // Where both log files are written. Relative paths resolve against the wrapper folder.
    "LogDirectory": "C:\\Logs\\nextjs-app",
    "MinimumLevel": "Information",
    "RetainedFileCountLimit": 31,
    "FileSizeLimitBytes": 104857600,
    "CrashContextLines": 50
  }
}
```

### 5.2 Options conventions

These follow the company code:

- Each options class has `public const string Key = "<Section>";`.
- Registration:
  ```csharp
  services.AddOptions<ServiceWrapperOptions>()
      .Bind(configuration.GetSection(ServiceWrapperOptions.Key))
      .ValidateDataAnnotations()
      .ValidateOnStart();
  ```
- Value types are nullable with `[Required]`, so a missing setting fails validation instead of silently binding `0`.
- TimeSpans use the company format `"0.00:00:10"`.
- DataAnnotations can't check the file system, so `ServiceWrapperOptionsValidator : IValidateOptions<ServiceWrapperOptions>` checks:
  - the executable resolves to an existing file;
  - the working directory exists;
  - `ShutdownTimeout` is between 1 and 25 seconds, which keeps the whole stop within the time the SCM allows.
- `HealthCheckOptions` implements `IValidatableObject`. **Only when `Enabled` is `true`**, it checks that:
  - `Url` is an absolute http/https URI;
  - all three timeouts are greater than zero, and `RequestTimeout` is no longer than `StartupTimeout`.

  When `Enabled` is `false`, `Url` may be empty or absent.
- `ENABLE_CONSOLE_LOGGING` is a top-level flag read with `GetValue<bool>`. This follows the company's `ENABLE_*` pattern, and the flag defaults to `false` when absent.
- Configuration sources: `appsettings.json`, then `appsettings.{Environment}.json`, then command line.
  - No prefixed environment-variable provider is added. Machine-level environment variables would leak into every wrapper install on the server, while `appsettings.json` next to the exe is meant to be the source of truth.
  - The default host builder's unprefixed environment-variable source stays as-is.

### 5.3 Path resolution (`Utilities\PathResolver`)

1. Expand `%VARIABLES%`.
2. If the value is rooted, use it.
3. If it contains a directory separator, resolve it against `AppContext.BaseDirectory`.
4. If it is a bare name (`node`), search `PATH` using `PATHEXT`.

The service account sees the **machine** PATH, so the resolved path is always logged at startup.

## 6. Runtime behavior

### 6.1 Startup sequence (`Program.cs`)

1. **Ctrl+C helper mode.** If the arguments are `--send-ctrl-c <pid>`, run the Ctrl+C helper (§6.4) and exit. No host is built in this mode.
2. **Bootstrap logger.** Read `SerilogOptions` from the raw configuration and resolve the log directory: the configured one, or `<wrapper folder>\Logs` if none is set. Never use `Environment.CurrentDirectory`, which is `C:\Windows\System32` under a service. Create the directory and start the bootstrap logger.
   - If the configured directory can't be created or written, write the error to the fallback directory and exit with code `13`.
3. **Build the host.** `Host.CreateApplicationBuilder(args)`, then `AddWindowsService()`, then `builder.Logging.ClearProviders()`, then `AddSerilog(...)`. Clearing the providers also removes the Event Log provider that `AddWindowsService()` adds, as decided.
4. **Validate every options section right after `Build()`** (`ValidateServiceWrapperOptions`), before the service reports *Running*.
   - All failures are collected and logged in one `Invalid configuration` line, and the wrapper exits with code `13`.
   - Values the binder can't convert (e.g. `"two seconds"` for a duration) count as configuration errors too.
   - `ValidateOnStart` stays registered as a safety net.
5. Wrap everything in `try / catch / finally`, ending with `Log.CloseAndFlush()`. Unlike the IMAPS/OSS template, the `catch` **sets a non-zero exit code** (`1064`), because a swallowed startup failure hides the problem from SCM recovery.
6. **Report early failures to the SCM.** A service process that exits before its host runs has never talked to the SCM. Windows then waits 30 s and records only "did not respond in a timely fashion", with no exit code (events 7009/7000). To avoid that, when running as a service, `StartupFailureService` briefly registers with the SCM and reports the stop with the real exit code (`13` or `1064`).

### 6.2 Supervision loop (`ProcessSupervisor : BackgroundService`)

```
StartAsync:
  log startup summary (app name, resolved exe, args, working dir, env var NAMES only, log dir)
  run "<exe> <VersionArguments>" once, log output             (optional)
  start child (§6.3)
  if HealthCheck.Enabled → wait for health URL (§6.7)          (service stays "Starting")
  else                   → log "Health check disabled", continue

ExecuteAsync loop:
  await child exit
  if stopping → break
  log "Process exited unexpectedly" with exit code, uptime, last N stderr lines
  decision = RestartPolicy.RecordExit(now)
  if decision.GiveUp → log, kill leftovers, flush logs, Environment.Exit(3)
  log "Restarting in {Delay}", wait delay (cancellable by stop)
  start child
  if HealthCheck.Enabled → wait for health URL (§6.7)
```

- **Starting a child:**
  - `UseShellExecute = false`, stdout/stderr redirected as UTF-8, `CreateNoWindow = true`. The child therefore gets its own hidden console, which the Ctrl+C helper attaches to.
  - **Do not** use `CREATE_NEW_PROCESS_GROUP`, because it disables Ctrl+C in the child.
  - Environment = the wrapper's environment plus `ServiceWrapper:EnvironmentVariables`.
  - Immediately after start, assign the child to the wrapper's Job Object (§6.5).
- **Port preflight:** if `PreflightPort` is set and in use (`IPGlobalProperties.GetActiveTcpListeners()`), the start attempt fails with "Port {Port} is already in use". The failure counts toward the restart policy.
- **Any exit not started by the wrapper is unexpected**, including exit code 0.
- **Why `Environment.Exit(3)`:** the default `BackgroundServiceExceptionBehavior.StopHost` stops cleanly with exit code 0, and the SCM only runs recovery actions for non-zero exits. Microsoft's Windows Service guidance recommends `Environment.Exit` for this case. Logs are flushed first.

### 6.3 Output capture

- stdout lines are logged at `Information`, stderr lines at `Warning`. Both use source context `LogSources.ChildProcess`, which routes them to the app log file (§7).
- ANSI color codes are stripped before logging.
- `OutputLineBuffer`: a thread-safe ring buffer that keeps the last `CrashContextLines` stdout and stderr lines. Its contents are attached to the "exited unexpectedly" entry so the cause of a crash is in one place.
- Before handling an exit, wait until both output streams reach end of file so the final lines aren't lost.

### 6.4 Graceful stop (Ctrl+C, then kill tree)

The wrapper can't send Ctrl+C itself. A Windows Service has no console, and attaching the wrapper to the child's console would interfere with its own console when run interactively. Instead, the wrapper **re-launches its own executable** as a short-lived helper:

```
Node.Application.Service.Wrapper.exe --send-ctrl-c <childPid>
  FreeConsole()
  AttachConsole(childPid)
  SetConsoleCtrlHandler(null, true)        // the helper ignores the event itself
  GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)
  exit 0 (or a non-zero code naming the failing call)
```

Stop sequence in `ProcessSupervisor.StopAsync`:

1. Mark the supervisor as stopping, so the exit isn't treated as a crash.
2. Run the helper and log its result.
3. Wait up to `ShutdownTimeout` for the child to exit, then log "Process stopped gracefully | ExitCode: {ExitCode}".
4. If the timeout expires, call `Process.Kill(entireProcessTree: true)` and log a warning.
5. `HostOptions.ShutdownTimeout` is set to `ShutdownTimeout + 5s`, so the host doesn't abandon the stop halfway.

Verified for the test app: the Next.js standalone server handles `SIGINT`, which Node raises on Windows for Ctrl+C, by running its cleanup and calling `process.exit(130)` (`next/dist/server/lib/start-server.js`). Exit code 130 during a stop is therefore normal.

### 6.5 Orphan protection (Job Object)

Each child gets its own Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, owned by its `ChildProcess` instance. Disposing a finished `ChildProcess` therefore also kills any descendants it left behind before the next restart. If the wrapper crashes or is killed from Task Manager, Windows closes the job handle and kills the child and all of its descendants. This keeps a dead wrapper's node process from holding the port and making every restart fail.

There is a small window between `Process.Start` and the job assignment. It doesn't matter here, because node doesn't spawn descendants within milliseconds of starting.

### 6.6 Restart policy (`RestartPolicy`)

This is pure logic with an injectable `TimeProvider`, so it can be unit tested.

- Keep the timestamps of exits within the last `RestartWindow`. Call their count `n`.
- If `n > MaxRestarts`, give up (exit code `1067`).
- Otherwise the delay is `min(InitialDelay × 2^(n-1), MaxDelay)`.
- Because old exits age out of the window, a long-running app that crashes once restarts quickly, and a crash loop backs off and then stops.

### 6.7 Health check at startup (`HealthEndpointChecker`)

This only runs when `HealthCheck:Enabled` is `true`. When it is `false`, the wrapper considers the app started as soon as the process launches, and logs `"Health check disabled | AppName: {AppName}"` once at startup.

When enabled, after every start (first start and each restart):

- Send `GET HealthCheck:Url` every `RetryInterval`, each request limited to `RequestTimeout`.
- Any 2xx response means the app is healthy. Log `"Health check passed | Url: {Url} | Elapsed: {Elapsed}"` and continue.
- If the child exits while the wrapper is waiting, handle it as a normal unexpected exit (§6.2).
- If `StartupTimeout` passes without a 2xx response:
  - log an error with the last status code or exception message;
  - kill the process tree;
  - count it as a failed start under the restart policy, so a never-healthy app ends in the crash-loop exit (code 1067) instead of running unhealthy forever.
- **Correction to the original design:** the service does *not* stay in *Starting* while the check runs. The .NET Windows Service lifetime reports *Running* before any hosted service starts. Holding *Starting* would need a custom host lifetime, which isn't worth it. The health check therefore decides whether a start *counts* (healthy, or killed and retried), not what services.msc shows.
- Uses an `HttpClient` from `IHttpClientFactory`. No resilience handler is added, because the wait loop is the retry.

## 7. Logging

| File | Contents | Format |
| --- | --- | --- |
| `<LogDirectory>\<AppName>-wrapper-YYYYMMDD.log` | Wrapper lifecycle: startup summary, starts, exits, restarts, stops, errors | `CompactJsonFormatter` (company standard) |
| `<LogDirectory>\<AppName>-app-YYYYMMDD.log` | Child stdout/stderr, line by line | Plain text: `{Timestamp:yyyy-MM-dd HH:mm:ss.fff zzz} [{Level:u3}] {Message:lj}{NewLine}` |
| Console (only when `ENABLE_CONSOLE_LOGGING` is `true`) | Both of the above, so the wrapper and the app can be followed live in a terminal | Plain text, with an `[app]` / `[wrapper]` prefix per line |

- **Console logging** is controlled by the `ENABLE_CONSOLE_LOGGING` flag, as in the IMAPS/OSS apps. It is `false` in `appsettings.json` and `true` in `appsettings.Development.json`. It is added to the bootstrap logger too, so startup errors are visible when running `dotnet run`.

- **Serilog is configured in code** (company pattern; no `ReadFrom.Configuration`), with two sub-loggers filtered on `SourceContext`. The app log uses plain text on purpose: it is the app's own output, and operators read it with Notepad or `Get-Content -Wait`.
- **Rolling:** daily, `rollOnFileSizeLimit: true`. **Retention is set explicitly** from `RetainedFileCountLimit` and `FileSizeLimitBytes`; the IMAPS/OSS apps rely on Serilog's defaults.
- **Enrichers:** `WithMachineName()`, `WithEnvironmentName()`, plus `AppName` as a property.
- **Minimum level:** from `SerilogOptions:MinimumLevel`. `Microsoft.*` is overridden to `Warning`.
- **Message style** follows the company's pipe-delimited structured templates, for example:
  - `"Process exited unexpectedly | AppName: {AppName} | ExitCode: {ExitCode} | Uptime: {Uptime}"`
  - `"Restarting process | Attempt: {Attempt} | Delay: {Delay}"`
- **Environment variable values are never logged**, only their names, since they may contain secrets.

## 8. Exit codes (`Constants\ExitCodes.cs`)

| Code | Meaning |
| --- | --- |
| 0 | Stopped normally |
| 13 | Invalid configuration (options validation or log directory). Windows: "The data is invalid." |
| 1064 | Unexpected failure (startup, or the supervisor itself failed). Windows: "An exception occurred in the service when handling the control request." |
| 1067 | Crash loop: gave up after `MaxRestarts` within `RestartWindow`. Windows: "The process terminated unexpectedly." |

## 9. Coding conventions

These are inferred from IMAPS/OSS code, taking the newer style where the two disagree:

- `net10.0`, `<Nullable>enable</Nullable>`, `<ImplicitUsings>enable</ImplicitUsings>`, `<RuntimeIdentifier>win-x64</RuntimeIdentifier>`.
  - `<AllowUnsafeBlocks>true</AllowUnsafeBlocks>`, required by `[LibraryImport]`.
  - `[assembly: SupportedOSPlatform("windows")]`.
- File-scoped namespaces, primary constructors, `private readonly` fields named `_camelCase`, `PascalCase` constants.
- `InvalidOperationException` for configuration and startup errors. Never `new Exception(...)`, and no empty `catch` blocks. Both are Kiuwan rules the company already remediates.
- DI registration in `Extensions\ServiceCollectionExtensions.AddServiceWrapper(this IServiceCollection, IConfiguration)`, ending with `return services;`.
- P/Invoke through `[LibraryImport]` in `Interop\NativeMethods.cs`. No extra interop packages.

### Packages (host)

All pinned to the latest stable versions when the project is created:

- `Microsoft.Extensions.Hosting`
- `Microsoft.Extensions.Hosting.WindowsServices`
- `Microsoft.Extensions.Options.DataAnnotations`
- `Serilog`
- `Serilog.Extensions.Hosting`
- `Serilog.Sinks.File`
- `Serilog.Sinks.Console`
- `Serilog.Formatting.Compact`
- `Microsoft.Extensions.Http`
- `Serilog.Enrichers.Environment`

### Packages (tests)

These match ParametersAPI/ProductsAPI:

- `xunit`
- `xunit.runner.visualstudio`
- `Moq`
- `AutoFixture`
- `coverlet.collector`
- `Microsoft.NET.Test.Sdk`

## 10. Implementation steps

Each step ends in a buildable state.

1. **Scaffold.**
   - Create the repo folder and `git init`.
   - Add `.gitignore`, `README.md`, `global.json`, the `.sln`, the worker project and the test project.
   - Add package references.
   - Done when: `dotnet build` and `dotnet test` pass with empty projects.
2. **Ctrl+C spike (highest risk, so first).**
   - Implement `NativeMethods`, `ConsoleCtrlSender` and the `--send-ctrl-c` helper mode.
   - Done when: a throwaway console run starts `node .next\standalone\server.js` from `nextjs-app`, the helper is invoked, and node exits with code 130 within a few seconds. This must hold with the wrapper running both from a terminal and as a real service.
3. **Options and validation.**
   - Add the options classes, `ServiceWrapperOptionsValidator`, `ExecutableResolver` and exit code `13` handling.
   - Done when: unit tests pass, and a bad path produces one clear log line and exit code 13.
4. **Logging.**
   - Add the bootstrap logger, the two-file Serilog setup, the fallback directory, retention settings and the `ENABLE_CONSOLE_LOGGING` console sink.
   - Done when: both files appear in the configured directory; nothing is written to the Event Log; console output appears only when the flag is `true`.
5. **Child process and output capture.**
   - Add `ChildProcess`, the Job Object, ANSI stripping and `OutputLineBuffer`.
   - Done when: the Next.js output appears in the app log, and killing the wrapper from Task Manager also kills node.
6. **Supervisor and restart policy.**
   - Add `ProcessSupervisor`, `RestartPolicy`, the give-up path (`Environment.Exit(3)`), and the graceful stop wired into `StopAsync`.
   - Done when: killing node triggers a restart with backoff; a crash loop exits with code 1067; stopping the service logs a graceful exit.
7. **Optional startup features.**
   - Add the version logging, the port preflight, `HealthCheckOptions` with its validator, and `HealthEndpointChecker`.
   - Done when: each one works when configured and is skipped when not. With `HealthCheck:Enabled = false` the wrapper never sends an HTTP request.
8. **README.**
   - Cover build, publish (`dotnet publish -c Release -r win-x64 --self-contained false`), deployment layout, and the `sc.exe create`/`sc.exe failure` commands for manual install.
   - The `sc.exe failure` command should also set `failureflag 1`, so recovery runs on non-zero exits.

## 11. Testing

**Unit tests** (xUnit + Moq, named `Method_ShouldX_WhenY`, with Arrange/Act/Assert comments):

- `RestartPolicy`: backoff sequence, delay cap, window expiry, giving up, using a fake `TimeProvider`.
- `ServiceWrapperOptionsValidator`: missing exe, missing directory, timeout bounds.
- `HealthCheckOptions` validation: these settings fail when `Enabled = true` but pass when `Enabled = false`:
  - an empty URL;
  - a relative URL;
  - a non-http URL;
  - a zero timeout.
- `HealthEndpointChecker` (with a fake `HttpMessageHandler`):
  - passes on the first 2xx;
  - keeps retrying on 5xx and connection errors;
  - times out after `StartupTimeout`;
  - is never called when disabled.
- `ExecutableResolver`: rooted path, relative path, `%VAR%` expansion, PATH lookup.
- `OutputLineBuffer`: capacity, ordering, concurrent writes.
- ANSI stripping.

**Integration tests** (`[Trait("Category", "Integration")]`, need `node` on PATH). These drive the real `ProcessSupervisor` against small `node -e` scripts:

| Scenario | Expected |
| --- | --- |
| Script handles SIGINT and exits | Stop completes within the timeout, logged as graceful |
| Script ignores SIGINT | Killed after `ShutdownTimeout`, warning logged |
| Script exits with code 1 every time | Backoff delays grow, then exit code 1067 |
| Script spawns a grandchild, then the test host disposes the Job Object | Both processes are gone |
| Script writes to stdout/stderr | Lines appear at Information/Warning, ANSI codes stripped |

**Manual acceptance against `nextjs-app`, installed as a real Windows Service:**

1. The service starts, and the page loads on the configured port.
2. `Stop-Service`: the wrapper log shows Ctrl+C sent and node exiting with code 130. No `node.exe` is left running.
3. End `node.exe` in Task Manager: restarted after the backoff delay.
4. End the wrapper in Task Manager: `node.exe` disappears too.
5. Point `Arguments` at a missing script: crash loop, exit code 1067, and SCM recovery restarts the service.
6. Point `ExecutablePath` at a missing file: a single clear validation error, exit code 13.
7. Start a second node on the same port first: the preflight error appears in the log.
8. Set an unwritable `LogDirectory`: the error is written in `<wrapper>\Logs`, exit code 13.
9. `HealthCheck:Enabled = true`: the wrapper log shows "Health check passed" after each start.
10. `HealthCheck:Enabled = true` with a wrong `Url` (e.g. `/api/missing`): startup timeout, process killed, restarts, and finally exit code 1067.
11. `HealthCheck:Enabled = false` with `Url` removed: the service starts normally, and "Health check disabled" is logged.
12. Run with `dotnet run` (Development): both logs appear live in the terminal. In the published build with the default settings, nothing is written to the console.

## 12. Risks

| Risk | Mitigation |
| --- | --- |
| Ctrl+C delivery from a service session behaves differently than in a terminal | Spike first (step 2), tested both ways. The kill-tree fallback guarantees the stop always completes. |
| SCM stop timeout shorter than `ShutdownTimeout` | `ShutdownTimeout` is capped at 25s by validation, and `HostOptions.ShutdownTimeout` is derived from it. |
| Service account can't find `node` | The resolved executable path is logged at startup. The README recommends an absolute `ExecutablePath`. |
| Service account can't write to the log directory | Startup check with fallback directory and exit code 13. The README documents the required ACL. |
| Output pipe buffers fill if reading stalls | Output is read asynchronously and continuously. Logging never blocks on the child. |

## 13. Ctrl+C spike results (step 2)

The spike harness drove the real `ChildProcess` and `CtrlCRequester` classes in five parent-process conditions:

- **console:** attached to a console;
- **newgroup:** new process group, which makes the process ignore Ctrl+C;
- **detached:** no console at all, as under a Windows service;
- **detached-newgroup:** no console and new process group;
- **hidden-newconsole:** own hidden console.

Every scenario passed in every condition, with 0 failures:

| Scenario | Result |
| --- | --- |
| Graceful SIGINT handler, 25 runs per condition | 125/125, exit within 220 ms |
| No SIGINT handler (default action) | Exits with `0xC000013A` (STATUS_CONTROL_C_EXIT) |
| App ignores SIGINT | Still running after 3 s; `KillTree` fallback stops it |
| Two children: Ctrl+C to one | The other is untouched |
| Grandchild on the same console | Both receive SIGINT and exit |
| Detached grandchild, `ChildProcess` disposed without kill | Job Object kills both |
| Helper aimed at an exited PID | Fails cleanly (exit 11, Win32 error 87) |
| Next.js standalone, 5 runs per condition | 25/25: healthy, exit code **130**, port released |
| Wrapper process crash (`FailFast`) | Child and grandchild die with it |

Findings that changed the code:

- **The "ignore Ctrl+C" flag is inherited by child processes.** A negative control proved it: when the parent ignores Ctrl+C and no reset is done, the child never exits. `ChildProcess.Start` therefore calls `SetConsoleCtrlHandler(null, false)` before every start. This is covered by a regression test.
- **`Process.Kill()` does not throw for an exited process**, so `KillTree` now checks `HasExited` first. This is also covered by a test.
- **Waiting for exit uses the `Exited` event, with a bounded 2 s wait for remaining output,** instead of `Process.WaitForExitAsync`. `WaitForExitAsync` would hang forever if a descendant kept the output pipe open.

The same scenarios now run as permanent xUnit integration tests (`[Trait("Category", "Integration")]`, skipped when node isn't installed). There are 13 tests, and 10 consecutive full runs had no failures.

**Service context verified.** The full harness was also run as `SYSTEM` in **session 0** with no console, through a temporary scheduled task launched detached in a new process group. These are the conditions of a Windows Service. All 11 scenarios passed again: 25/25 graceful stops, and 5/5 Next.js stops with exit code 130.

## 14. Real-service acceptance results

**Setup:**

- The Release build was published framework-dependent `win-x64`.
- It was deployed in the one-install-per-app layout (`.deploy\nextjs-app\{app,wrapper}`) and installed with `sc.exe` as **LocalSystem**.
- Recovery was configured as `restart/5000/restart/5000//0` with `failureflag 1`.
- The app was the `nextjs-app` standalone build.

| # | Scenario | Result |
| --- | --- | --- |
| A1 | Start the service | Running; the node child runs as SYSTEM in session 0; health check passed; page returns 200 |
| A2 | `Stop-Service` | Stopped in **266 ms**: Ctrl+C sent, Next.js exited **130**, no node left, port released, SCM exit code 0 |
| C | Kill `node.exe` | "Process exited unexpectedly" logged with the last 50 output lines; restarted after 2 s; healthy again |
| D | Kill the wrapper process | node died with it (Job Object); SCM event 7031; recovery restarted the service within 5 s |
| E | App crashes on every start (`Arguments: missing.js`) | Gave up after `MaxRestarts`; **SCM exit code 1067**; recovery restarted the service once, then took no action, as configured. The crash entry contained Node's full `MODULE_NOT_FOUND` stack |
| F | Missing executable, invalid `RequestTimeout` and invalid `MinimumLevel` together | One log line listing all three problems; **SCM exit code 13**; no 30 s start timeout |
| F2 | Unconvertible value (`"two seconds"`) | Logged as invalid configuration; SCM exit code 13 |
| G | `PreflightPort` already taken | "Port is already in use \| Port: 3000"; no node started; exit code 1067 |
| H | `LogDirectory` on a non-existent drive | Error written to `wrapper\Logs`; SCM exit code 13 |
| I | Wrong health URL (`/api/missing`) | "Health check failed, killing process \| LastResult: HTTP 404"; killed twice; exit code 1067; no node left |
| J | `HealthCheck:Enabled = false` with `Url` removed | Starts normally; "Health check disabled" logged; graceful stop |
| K | `Stop-Service` during a 30 s restart delay | Stopped in 257 ms, exit code 0 |
| L | `Stop-Service` while waiting for the health check | Stopped in 268 ms; Ctrl+C delivered; graceful exit 130; no node left |

**Fixes made because of these tests:**

1. **Reading the exit code after `RunAsync`** threw `ObjectDisposedException`, because `RunAsync` disposes the host. The exit state is now resolved before the run.
2. **Configuration errors** surfaced either as "did not respond in a timely fashion" or as 1067, depending on where they were caught. Two changes fixed this:
   - options are now validated before the run;
   - the early-failure path reports the exit code through `StartupFailureService`.
3. **`localhost` in `HealthCheck:Url`** made each check take about 2 s. Next.js binds IPv4 `0.0.0.0`, while `localhost` tries `::1` first, and on Windows each refused attempt takes about 2 s. The default is now `127.0.0.1`.
4. **`appsettings.Development.json`** is no longer published.

## 15. Decision: exit codes use Win32 codes whose text fits

The SCM describes a service's exit code with the matching Win32 error text, in event 7023 and `sc.exe query`. The first codes (1/2/3) read as "Incorrect function", "The system cannot find the file specified" and "The system cannot find the path specified", which misled operators.

**Decided:** use Win32 codes whose standard text matches what happened.

| Code | Meaning | Windows text |
| --- | --- | --- |
| `13` | Invalid configuration | "The data is invalid." |
| `1064` | Unexpected failure | "An exception occurred in the service when handling the control request." |
| `1067` | Crash loop | "The process terminated unexpectedly." |

Re-verified on the real service: event 7023 now reads "terminated with the following error: The data is invalid." and "…: The process terminated unexpectedly.". A normal stop still reports 0.

**Trade-off:** Windows also reports 1067 when the wrapper process itself is killed or crashes. To tell the two apart, look at the wrapper log: a crash loop always ends with `Giving up: too many exits`.
## 16. Later phases (from the brainstorm)

- Periodic HTTP health probe that restarts a hung app after N failures. It adds `Interval` and `FailureThreshold` to the existing `HealthCheck` section and is controlled by the same `Enabled` switch.
- Memory ceiling and scheduled daily restart.
- Wrapper status endpoint `/health/<app-name>`.
- `Scripts\Install-Service.ps1` / `Uninstall-Service.ps1`:
  - service account (virtual account or gMSA);
  - recovery actions;
  - delayed auto start;
  - log directory ACL.
- `azure-pipelines.yml` (`windows-latest`, build, test, CodeQL, publish artifact), matching the company pipelines.
