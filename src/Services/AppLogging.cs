using System.IO;
using Serilog;

namespace WarpAgent.Services;

/// <summary>
/// Serilog bootstrap: devtools-style readable lines — timestamp, level, source,
/// message, full exception — into a daily-rolling file under ~/.warp-agent/logs
/// plus the debug output.
/// </summary>
public static class AppLogging
{
    private const string Template =
        "{Timestamp:yyyy-MM-dd HH:mm:ss.fff} [{Level:u3}] {SourceContext}: {Message:lj}{NewLine}{Exception}";

    public static void Initialize()
    {
        // Environment.Exit (SIGTERM path) skips Main's finally, so flush via ProcessExit too
        AppDomain.CurrentDomain.ProcessExit += (_, _) => Shutdown();
        try
        {
            Directory.CreateDirectory(AppPaths.LogsDir);
            Log.Logger = BuildLogger(writeToFile: true);
        }
        catch
        {
            // logging is best-effort; without a writable ~/.warp-agent only debug output remains
            Log.Logger = BuildLogger(writeToFile: false);
        }
        Log.Information("WarpAgent starting — logs in {LogsDir}", AppPaths.LogsDir);
    }

    private static ILogger BuildLogger(bool writeToFile)
    {
        var config = new LoggerConfiguration()
            .MinimumLevel.Debug()
            .WriteTo.Debug(outputTemplate: Template);
        if (writeToFile)
            config = config.WriteTo.File(
                Path.Combine(AppPaths.LogsDir, "warpagent-.log"),
                outputTemplate: Template,
                rollingInterval: RollingInterval.Day,
                retainedFileCountLimit: 14,
                shared: true);
        return config.CreateLogger();
    }

    /// <summary>Flushes buffered lines to disk; safe to call more than once.</summary>
    public static void Shutdown()
    {
        try
        {
            Log.CloseAndFlush();
        }
        catch
        {
        }
    }
}
