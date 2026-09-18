using System.IO;
using Serilog;

namespace WarpAgent.Services;

/// <summary>
/// Global per-user locations: everything lives in ~/.warp-agent — settings.json,
/// ui.json and the rolling logs. On first run the legacy ~/.config/opencode-agent
/// files migrate over, so workspaces, pins and settings survive the move.
/// </summary>
public static class AppPaths
{
    public static readonly string ConfigDir = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".warp-agent");
    public static readonly string LogsDir = Path.Combine(ConfigDir, "logs");
    public static readonly string SettingsFile = Path.Combine(ConfigDir, "settings.json");
    public static readonly string UiStateFile = Path.Combine(ConfigDir, "ui.json");

    private static readonly string LegacyDir = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "opencode-agent");

    /// <summary>Copies a legacy opencode-agent file into ~/.warp-agent when the new copy does not exist yet.</summary>
    public static void MigrateLegacyFile(string fileName)
    {
        try
        {
            var target = Path.Combine(ConfigDir, fileName);
            if (File.Exists(target))
                return;
            var legacy = Path.Combine(LegacyDir, fileName);
            if (!File.Exists(legacy))
                return;
            Directory.CreateDirectory(ConfigDir);
            File.Copy(legacy, target);
            Log.Information("migrated {File} from {LegacyDir} to {ConfigDir}", fileName, LegacyDir, ConfigDir);
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "could not migrate legacy {File}", fileName);
        }
    }
}
