using System.IO;
using System.Text.Json;
using Serilog;

namespace WarpAgent.Services;

/// <summary>
/// User preferences, persisted separately from UiState so chat-state churn never
/// rewrites them. Lives in ~/.warp-agent/settings.json, next to ui.json.
/// </summary>
public sealed class AppSettings
{
    public const string DefaultExecutable = "opencode";

    /// <summary>Binary or full path launched as `opencode serve`; PATH-resolved when bare.</summary>
    public string OpenCodeExecutable { get; set; } = DefaultExecutable;
    public string? DefaultModel { get; set; }
    public string? DefaultVariant { get; set; }
    public string? DefaultAgent { get; set; }

    /// <summary>Null = never chosen; auto-allow defaults to on until explicitly unticked.</summary>
    public bool? AutoAllowAlways { get; set; }

    /// <summary>True = auto-allow answers instantly; false = wait out the random delay first.</summary>
    public bool AutoAllowInstant { get; set; }

    /// <summary>Random auto-allow wait in seconds: working-hours range; lunch and off hours use the away range.</summary>
    public int AutoAllowDefaultMinSeconds { get; set; } = 1;
    public int AutoAllowDefaultMaxSeconds { get; set; } = 15;

    /// <summary>Range for lunch and off hours, when nobody is assumed to be watching.</summary>
    public int AutoAllowAwayMinSeconds { get; set; } = 60;
    public int AutoAllowAwayMaxSeconds { get; set; } = 600;

    /// <summary>Local hours: lunch is [start, end); off hours run from start to WorkdayStartHour next morning.</summary>
    public int LunchStartHour { get; set; } = 12;
    public int LunchEndHour { get; set; } = 14;
    public int OffHoursStartHour { get; set; } = 18;
    public int WorkdayStartHour { get; set; } = 8;

    /// <summary>Text-size variant: "compact" / "normal" / "large"; null = normal.</summary>
    public string? FontScale { get; set; }

    /// <summary>Accent preset key from AccentTheme; null = green.</summary>
    public string? Accent { get; set; }

    /// <summary>Tooltip visibility; off hides every hover tooltip app-wide.</summary>
    public bool ShowToolTips { get; set; } = true;

    private static string FilePath => AppPaths.SettingsFile;

    public static AppSettings Load()
    {
        AppPaths.MigrateLegacyFile("settings.json");
        try
        {
            if (File.Exists(FilePath))
                return JsonSerializer.Deserialize<AppSettings>(File.ReadAllText(FilePath)) ?? new AppSettings();
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "settings.json unreadable; falling back to defaults");
        }
        return new AppSettings();
    }

    public void Save()
    {
        try
        {
            Directory.CreateDirectory(AppPaths.ConfigDir);
            var tmp = FilePath + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(this, new JsonSerializerOptions { WriteIndented = true }));
            File.Move(tmp, FilePath, overwrite: true);
        }
        catch (Exception ex)
        {
            // a failed settings write is not worth interrupting the chat
            Log.Warning(ex, "settings.json write failed");
        }
    }
}
