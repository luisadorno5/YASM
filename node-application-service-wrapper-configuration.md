# Node.Application.Service.Wrapper: Configuration Reference

All settings live in `appsettings.json`, in the same folder as `Node.Application.Service.Wrapper.exe`. Each installed app has its own wrapper folder and its own `appsettings.json`.

**After editing, restart the service.** Settings are read once at startup.

When settings are invalid, the service does not start the application. Instead it:

1. writes **one** `Invalid configuration` line to the wrapper log, listing every problem it found;
2. stops with exit code `13`, which Windows records for the service.

If the log directory itself can't be used, that error goes to `<wrapper folder>\Logs`.

## Value formats

| Type | Format | Example |
| --- | --- | --- |
| Duration | `d.hh:mm:ss` | `"0.00:00:10"` = 10 seconds, `"0.00:01:00"` = 1 minute |
| Path | Absolute, relative to the wrapper folder, or containing `%ENV_VAR%` | `"C:\\Apps\\site\\app"`, `"..\\app"`, `"%ProgramFiles%\\nodejs\\node.exe"` |

Backslashes in JSON strings must be doubled (`\\`).

## `ServiceWrapper`

The application this wrapper starts and supervises.

| Setting | Required | Default | Description |
| --- | --- | --- | --- |
| `AppName` | Yes | — | Short label for the application. Used in log file names (`<AppName>-wrapper-*.log`, `<AppName>-app-*.log`) and in every log entry. It does not need to match the Windows service name. Use letters, digits and dashes. |
| `ExecutablePath` | Yes | — | The program to run. It can be an absolute path, a path relative to the wrapper folder, a path containing `%ENV_VAR%`, or a bare name such as `node`, which is looked up in the **machine** PATH. The resolved path is logged at startup. Prefer an absolute path on servers. |
| `Arguments` | No | empty | Command-line arguments passed to the program, e.g. `server.js`. |
| `WorkingDirectory` | Yes | — | Folder the program runs in. For a Next.js standalone app, this is the folder that contains `server.js`. Relative paths resolve against the wrapper folder. |
| `EnvironmentVariables` | No | none | Name/value pairs added to the program's environment, on top of the wrapper's own environment. For Next.js these are typically `PORT`, `HOSTNAME` and `NODE_ENV`. **Only the names are logged**, never the values, so secrets placed here stay out of the logs. |
| `ShutdownTimeout` | Yes | — | When the service stops, the wrapper sends Ctrl+C to the program and waits this long for it to exit. After that it kills the program and everything it started. Must be between 1 and 25 seconds, so the whole stop fits within the time Windows allows. Next.js normally exits within milliseconds. |
| `VersionArguments` | No | empty | If set, the wrapper runs `<ExecutablePath> <VersionArguments>` once at startup and logs the output, e.g. `--version` logs the Node.js version. Leave empty to skip. |
| `PreflightPort` | No | none | If set, the wrapper checks before each start whether this TCP port is already in use. If it is, the start attempt fails with "Port is already in use" instead of a confusing application error, and the failure counts toward `RestartPolicy`. Usually the same value as `PORT`. Remove or set to `null` to skip. |

## `RestartPolicy`

What happens when the application exits on its own. Any exit that the wrapper did not request counts, including exit code 0.

| Setting | Required | Default | Description |
| --- | --- | --- | --- |
| `InitialDelay` | Yes | — | Wait before the first restart. |
| `MaxDelay` | Yes | — | Upper limit for the wait between restarts. Must be at least `InitialDelay`. |
| `MaxRestarts` | Yes | — | How many exits are tolerated within `RestartWindow`. One more than this and the wrapper gives up: it stops the service with exit code `1067`, so the Windows service recovery settings decide what happens next. `0` means never restart. |
| `RestartWindow` | Yes | — | Period over which exits are counted. Exits older than this are forgotten. |

### How the delay grows

The delay doubles with each exit inside the window: `InitialDelay × 2^(n-1)`, capped at `MaxDelay`, where `n` is the number of exits in the last `RestartWindow`.

With the defaults (2 s, 1 min cap, 5 restarts, 10 min window):

- the wrapper waits 2 s, 4 s, 8 s, 16 s, then 32 s before restarts 1–5;
- a 6th exit within 10 minutes stops the service;
- an app that crashes once a day always restarts after 2 s.

## `HealthCheck`

Optional startup check that confirms the application is actually serving requests.

| Setting | Required | Default | Description |
| --- | --- | --- | --- |
| `Enabled` | Yes | — | `true`: after every start (including restarts), wait until `Url` returns a 2xx response, and log "Health check passed". `false`: no HTTP requests are made, and the other settings in this section are ignored. Use `false` for applications without a health endpoint. services.msc shows *Running* as soon as the wrapper itself is up, either way. |
| `Url` | When `Enabled` | — | Absolute `http`/`https` URL to call, e.g. `http://127.0.0.1:3000/api/health`. **Use `127.0.0.1` rather than `localhost`** when the app listens on `0.0.0.0` (IPv4 only). `localhost` tries IPv6 first, and on Windows every refused attempt adds about 2 seconds. |
| `StartupTimeout` | When `Enabled` | — | How long to wait for the first 2xx response after a start. If it never arrives, the wrapper kills the application and treats it as a failed start, so `RestartPolicy` applies. An application that never becomes healthy therefore ends in exit code `1067` instead of running unhealthy forever. |
| `RetryInterval` | When `Enabled` | — | Wait between attempts while waiting for the first 2xx. |
| `RequestTimeout` | When `Enabled` | — | Time limit for each individual request. Must not be longer than `StartupTimeout`. |

## `ENABLE_CONSOLE_LOGGING`

| Setting | Required | Default | Description |
| --- | --- | --- | --- |
| `ENABLE_CONSOLE_LOGGING` | No | `false` | Also writes both logs to the console, each line marked `[wrapper]` or `[app]`. Only useful when running the wrapper from a terminal (`dotnet run`). Keep `false` on servers, where the service has no console. |

## `SerilogOptions`

File logging. The wrapper writes two daily log files:

| File | Contents | Format |
| --- | --- | --- |
| `<AppName>-wrapper-YYYYMMDD.log` | The wrapper's own events: startup summary, starts, exits with exit code and uptime, restarts, stops, configuration errors | One JSON object per line (company standard) |
| `<AppName>-app-YYYYMMDD.log` | Everything the application writes to stdout (`INF`) and stderr (`WRN`), one entry per line, color codes removed | Plain text |

| Setting | Required | Default | Description |
| --- | --- | --- | --- |
| `LogDirectory` | No | `<wrapper folder>\Logs` | Folder for both log files. Created if missing. Relative paths resolve against the wrapper folder. The service account needs write access. If the folder can't be created or written to, the error is written to `<wrapper folder>\Logs` and the service stops with exit code `13`. |
| `MinimumLevel` | No | `Information` | Least severe level written: `Verbose`, `Debug`, `Information`, `Warning`, `Error` or `Fatal`. Application output is logged at `Information` (stdout) and `Warning` (stderr), so setting this above `Information` hides normal application output. |
| `RetainedFileCountLimit` | No | `31` | Number of files kept per log. Older files are deleted. With daily files, `31` keeps about a month of logs. |
| `FileSizeLimitBytes` | No | `104857600` (100 MB) | When a file reaches this size, a new one is started for the same day (`..._001.log`). Together with `RetainedFileCountLimit`, this caps disk usage at roughly 2 × limit × count. |
| `CrashContextLines` | No | `50` | Number of the application's most recent output lines included in the wrapper log entry when the application exits unexpectedly. This puts the cause of a crash in one place. `0` turns it off. |

## Exit codes

When the service stops on its own, Windows records its exit code. It appears in the System event log, written by the Service Control Manager, and in `sc.exe query` as `WIN32_EXIT_CODE`.

| Code | Meaning |
| --- | --- |
| `0` | Stopped normally |
| `13` | Invalid configuration, or the log directory can't be used. Windows shows *"The data is invalid."* |
| `1064` | Unexpected failure; see the wrapper log. Windows shows *"An exception occurred in the service when handling the control request."* |
| `1067` | The application kept crashing: more than `MaxRestarts` exits within `RestartWindow`. Windows shows *"The process terminated unexpectedly."* |

Windows also reports `1067` when the wrapper process itself is killed. If the wrapper log ends with `Giving up: too many exits`, it was the crash loop.

For Windows to run its recovery actions on codes `13` and `1067`, the service needs `sc.exe failureflag <name> 1`.

## `appsettings.Development.json`

This file is only read when `DOTNET_ENVIRONMENT=Development`, which `dotnet run` sets through `launchSettings.json`. A Windows service doesn't set it, so the file is ignored on servers. It overrides only what differs on a developer machine:

- runs the sandbox `nextjs-app`;
- turns on console logging;
- writes logs to `Logs` next to the exe;
- logs at `Debug`.
