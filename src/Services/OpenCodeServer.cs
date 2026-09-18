using System.Diagnostics;
using System.Net;
using System.Net.Http;
using System.Net.Sockets;
using System.Runtime.InteropServices;

namespace WarpAgent.Services;

/// <summary>
/// Spawns <c>opencode serve</c> for a working folder, or attaches when the chosen
/// port already answers. Bash/edit/webfetch permissions are forced to "ask" through
/// OPENCODE_CONFIG_CONTENT (verified against opencode 1.18.31; there is no --config
/// flag on serve) so the concept always exercises the permission flow without
/// touching the user's global config or the working folder.
/// </summary>
public sealed class OpenCodeServer
{
    public const string AskConfig =
        """{"$schema":"https://opencode.ai/config.json","permission":{"edit":"ask","bash":"ask","webfetch":"ask"}}""";

    private Process? _process;
    private readonly List<PosixSignalRegistration> _signalRegistrations = [];
    private readonly HttpClient _http = new() { Timeout = TimeSpan.FromSeconds(2) };

    public string BaseUrl { get; private set; } = "";
    public bool Spawned { get; private set; }
    public bool IsRunning { get; private set; }

    public async Task StartAsync(string executable, string folder, int port, Action<string> log, CancellationToken ct)
    {
        BaseUrl = $"http://127.0.0.1:{port}";
        if (await IsHealthyAsync(ct))
        {
            Spawned = false;
            IsRunning = true;
            log($"attaching to a healthy opencode server at {BaseUrl}");
            return;
        }

        var psi = new ProcessStartInfo
        {
            // Windows: `opencode` is usually a PATH shim (.cmd); cmd /c resolves it
            // while keeping UseShellExecute=false so env vars, redirects and the
            // tree kill keep working.
            // cmd /c with a quoted path needs the whole argument string quoted again
            FileName = OperatingSystem.IsWindows() ? "cmd.exe" : executable,
            Arguments = OperatingSystem.IsWindows()
                ? executable.Contains(' ')
                    ? $"/c \"{executable} serve --hostname 127.0.0.1 --port {port}\""
                    : $"/c {executable} serve --hostname 127.0.0.1 --port {port}"
                : $"serve --hostname 127.0.0.1 --port {port}",
            // serve inherits its config and project scope from the working directory.
            WorkingDirectory = Directory.Exists(folder) ? folder : Environment.CurrentDirectory,
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true,
        };
        psi.Environment["OPENCODE_CONFIG_CONTENT"] = AskConfig;
        psi.Environment.Remove("ELECTRON_RUN_AS_NODE");

        _process = Process.Start(psi) ?? throw new InvalidOperationException($"'{executable}' failed to start.");
        // WM close binds send SIGTERM without a Wayland close event: OnClosing and
        // ProcessExit never run, so the signal must be handled here or the serve
        // child orphans (reproduced; ProcessExit is NOT raised on POSIX SIGTERM).
        if (!OperatingSystem.IsWindows())
        {
            // the registrations MUST be kept referenced: a collected registration
            // silently unregisters its handler
            _signalRegistrations.Add(PosixSignalRegistration.Create(PosixSignal.SIGTERM, OnPosixSignal));
            _signalRegistrations.Add(PosixSignalRegistration.Create(PosixSignal.SIGINT, OnPosixSignal));
        }
        AppDomain.CurrentDomain.ProcessExit += (_, _) => Stop();
        _process.OutputDataReceived += (_, e) => { if (e.Data is not null) log(e.Data); };
        _process.ErrorDataReceived += (_, e) => { if (e.Data is not null) log(e.Data); };
        _process.BeginOutputReadLine();
        _process.BeginErrorReadLine();
        Spawned = true;

        var deadline = TimeSpan.FromSeconds(25);
        var start = Stopwatch.StartNew();
        while (start.Elapsed < deadline)
        {
            ct.ThrowIfCancellationRequested();
            if (_process.HasExited)
                throw new InvalidOperationException($"opencode serve exited with code {_process.ExitCode}.");
            if (await IsHealthyAsync(ct))
            {
                IsRunning = true;
                return;
            }
            await Task.Delay(300, ct);
        }

        Stop();
        throw new TimeoutException("opencode serve did not become healthy within 25 s.");
    }

    /// <summary>Every workspace runs its own serve instance, so each start grabs a free port.</summary>
    public static int FindFreePort()
    {
        var listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start();
        try
        {
            return ((IPEndPoint)listener.LocalEndpoint).Port;
        }
        finally
        {
            listener.Stop();
        }
    }

    public async Task<bool> IsHealthyAsync(CancellationToken ct)
    {
        try
        {
            using var resp = await _http.GetAsync($"{BaseUrl}/global/health", ct);
            return resp.IsSuccessStatusCode;
        }
        catch
        {
            return false;
        }
    }

    private void OnPosixSignal(PosixSignalContext context)
    {
        Stop();
        context.Cancel = true;
        Environment.Exit(0);
    }

    public void Stop()
    {
        if (Spawned && _process is { HasExited: false } p)
        {
            try
            {
                p.Kill(entireProcessTree: true);
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine($"[server] kill failed: {ex.Message}");
            }
        }
        _process = null;
        Spawned = false;
        IsRunning = false;
    }
}
