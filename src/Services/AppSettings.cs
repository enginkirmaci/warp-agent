using System.IO;
using System.Text.Json;

namespace WarpAgent.Services;

/// <summary>
/// User preferences, persisted separately from UiState so chat-state churn never
/// rewrites them. Lives in settings.json next to ui.json.
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

    private static string DirectoryPath => Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
        "opencode-agent");
    private static string FilePath => Path.Combine(DirectoryPath, "settings.json");

    public static AppSettings Load()
    {
        try
        {
            if (File.Exists(FilePath))
                return JsonSerializer.Deserialize<AppSettings>(File.ReadAllText(FilePath)) ?? new AppSettings();
        }
        catch
        {
            // corrupt settings fall back to defaults
        }
        return new AppSettings();
    }

    public void Save()
    {
        try
        {
            Directory.CreateDirectory(DirectoryPath);
            var tmp = FilePath + ".tmp";
            File.WriteAllText(tmp, JsonSerializer.Serialize(this, new JsonSerializerOptions { WriteIndented = true }));
            File.Move(tmp, FilePath, overwrite: true);
        }
        catch
        {
            // a failed settings write is not worth interrupting the chat
        }
    }
}
