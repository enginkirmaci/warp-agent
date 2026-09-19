using System.Diagnostics;
using System.Globalization;
using Serilog;

namespace WarpAgent.Services;

/// <summary>
/// Follows the desktop text scaling GTK apps honor — gsettings
/// org.gnome.desktop.interface text-scaling-factor (omarchy's panel text-size
/// control writes it). The initial value is read synchronously with a short
/// timeout so the first frame is already sized right; a gsettings monitor
/// subprocess feeds live changes into <see cref="FontScale"/> afterwards.
/// </summary>
public static class SystemFontScale
{
    private const string Key = "text-scaling-factor";
    private static Process? _monitor;

    public static void Initialize()
    {
        if (!OperatingSystem.IsLinux())
            return;
        AppDomain.CurrentDomain.ProcessExit += (_, _) => KillMonitor();
        if (ReadFactor() is { } factor)
            FontScale.SetSystemFactor(factor);
        else
            Log.Debug("system text scaling unreadable; keeping factor 1.0");
        StartMonitor();
    }

    private static double? ReadFactor()
    {
        try
        {
            using var proc = Process.Start(new ProcessStartInfo(
                "gsettings", $"get org.gnome.desktop.interface {Key}")
            {
                UseShellExecute = false,
                RedirectStandardOutput = true,
                CreateNoWindow = true,
            });
            if (proc is null)
                return null;
            if (!proc.WaitForExit(1500))
            {
                proc.Kill();
                return null;
            }
            return Parse(proc.StandardOutput.ReadToEnd());
        }
        catch (Exception ex)
        {
            Log.Debug(ex, "gsettings get failed");
            return null;
        }
    }

    private static void StartMonitor()
    {
        try
        {
            _monitor = Process.Start(new ProcessStartInfo(
                "gsettings", $"monitor org.gnome.desktop.interface")
            {
                UseShellExecute = false,
                RedirectStandardOutput = true,
                CreateNoWindow = true,
            });
            if (_monitor is null)
                return;
            var stdout = _monitor.StandardOutput;
            _ = Task.Run(async () =>
            {
                while (await stdout.ReadLineAsync() is { } line)
                {
                    if (Parse(line) is { } factor)
                        FontScale.SetSystemFactor(factor);
                }
            });
        }
        catch (Exception ex)
        {
            Log.Debug(ex, "gsettings monitor unavailable; system text scale frozen at startup value");
            KillMonitor();
        }
    }

    /// <summary>Accepts bare "1.1667" (gsettings get) and "key: 1.1667" (gsettings monitor) lines.</summary>
    private static double? Parse(string text)
    {
        var idx = text.IndexOf(Key, StringComparison.Ordinal);
        if (idx >= 0)
            text = text[(idx + Key.Length)..];
        text = text.TrimStart(':', ' ', '\t').TrimEnd();
        return double.TryParse(text, NumberStyles.Float, CultureInfo.InvariantCulture, out var value) && value > 0
            ? value
            : null;
    }

    private static void KillMonitor()
    {
        try { _monitor?.Kill(); }
        catch { /* already gone */ }
        _monitor = null;
    }
}
