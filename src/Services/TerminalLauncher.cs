using System.Diagnostics;
using System.IO;
using Serilog;

namespace WarpAgent.Services;

/// <summary>
/// Spawns a terminal emulator running the opencode TUI in a folder. Terminal
/// detection probes PATH for the usual suspects and picks the first hit — each
/// emulator spells its working-directory and exec flags differently. On Windows
/// the command goes to Windows Terminal, falling back to `cmd start`.
/// </summary>
public static class TerminalLauncher
{
    private static readonly ILogger Log = Serilog.Log.ForContext(typeof(TerminalLauncher));

    // first terminal found on PATH wins; order puts the desktops this app actually runs on first
    private static readonly (string Name, Func<string, string, string[]> Args)[] Terminals =
    [
        ("alacritty", (dir, exe) => ["--working-directory", dir, "-e", exe]),
        ("ghostty", (dir, exe) => [$"--working-directory={dir}", "-e", exe]),
        ("foot", (dir, exe) => [$"--working-directory={dir}", exe]),
        ("kitty", (dir, exe) => ["--directory", dir, exe]),
        ("wezterm", (dir, exe) => ["start", "--cwd", dir, "--", exe]),
        ("gnome-terminal", (dir, exe) => [$"--working-directory={dir}", "--", exe]),
        ("konsole", (dir, exe) => ["--workdir", dir, "-e", exe]),
        ("xterm", (dir, exe) => ["-e", exe]),
    ];

    public static void OpenInOpenCode(string folder, string executable)
    {
        if (OperatingSystem.IsWindows())
        {
            OpenWindows(folder, executable);
            return;
        }

        foreach (var (name, args) in Terminals)
        {
            if (ResolveOnPath(name) is not { } terminal)
                continue;
            var psi = new ProcessStartInfo(terminal, args(folder, executable))
            {
                // belt-and-braces for xterm, whose only cwd is the inherited one
                WorkingDirectory = Directory.Exists(folder) ? folder : Environment.CurrentDirectory,
                UseShellExecute = false,
            };
            Process.Start(psi)?.Dispose();
            Log.Information("launched {Terminal} running {Executable} in {Folder}", name, executable, folder);
            return;
        }

        throw new InvalidOperationException("No terminal emulator found on PATH.");
    }

    private static void OpenWindows(string folder, string executable)
    {
        if (ResolveOnPath("wt.exe") is not null)
        {
            var exe = executable.Contains(' ') ? $"\"{executable}\"" : executable;
            Process.Start(new ProcessStartInfo("wt.exe", $"-d \"{folder}\" {exe}") { UseShellExecute = false })?.Dispose();
            return;
        }

        // `start` eats the first quoted argument as the window title, so one is required
        // even before a quoted path
        var command = $"/c start \"opencode\" /D \"{folder}\" {executable}";
        if (executable.Contains(' '))
            command = $"/c start \"opencode\" /D \"{folder}\" \"{executable}\"";
        Process.Start(new ProcessStartInfo("cmd.exe", command) { UseShellExecute = false })?.Dispose();
    }

    private static string? ResolveOnPath(string name)
    {
        var pathEnv = Environment.GetEnvironmentVariable("PATH");
        if (string.IsNullOrEmpty(pathEnv))
            return null;
        var separator = OperatingSystem.IsWindows() ? ';' : ':';
        foreach (var dir in pathEnv.Split(separator, StringSplitOptions.RemoveEmptyEntries))
        {
            var full = Path.Combine(dir.Trim('"'), name);
            if (!File.Exists(full))
                continue;
            if (OperatingSystem.IsWindows() || (File.GetUnixFileMode(full) & UnixFileMode.OtherExecute) != 0)
                return full;
        }
        return null;
    }
}
