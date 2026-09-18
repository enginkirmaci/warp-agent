using System.IO;
using System.Text.Json;

namespace WarpAgent.Services;

/// <summary>
/// Client-side UI state (workspaces, pins, last selection) — the opencode server keeps
/// sessions but knows nothing about workspaces, pins or which chat was open. One small
/// JSON file under the user's application-data folder.
/// </summary>
public sealed class UiState
{
    public Dictionary<string, bool> Pins { get; set; } = new();

    /// <summary>Added workspace folders; each tracks only the sessions this app created.</summary>
    public List<WorkspaceState> Workspaces { get; set; } = new();
    public string? ActiveWorkspace { get; set; }
    public string? Model { get; set; }
    public string? Variant { get; set; }
    public string? Agent { get; set; }

    /// <summary>Legacy single working folder; read once to seed the first workspace, then ignored.</summary>
    public string? Folder { get; set; }

    /// <summary>Null = never chosen; auto-allow defaults to on (always-allow) until explicitly unticked.</summary>
    public bool? AutoAllowAlways { get; set; }

    private static string FilePath => Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
        "opencode-agent",
        "ui.json");

    public static UiState Load()
    {
        try
        {
            if (File.Exists(FilePath))
                return JsonSerializer.Deserialize<UiState>(File.ReadAllText(FilePath)) ?? new UiState();
        }
        catch
        {
            // corrupt or unreadable state must never block the app from starting
        }
        return new UiState();
    }

    public void Save()
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(FilePath)!);
            File.WriteAllText(FilePath, JsonSerializer.Serialize(this, new JsonSerializerOptions { WriteIndented = true }));
        }
        catch
        {
            // a failed state write is not worth interrupting the chat
        }
    }
}

/// <summary>One workspace folder plus the ids of the sessions created from this app under it.</summary>
public sealed class WorkspaceState
{
    public string Path { get; set; } = "";
    public List<string> Sessions { get; set; } = new();
    public string? ActiveSession { get; set; }

    /// <summary>Chats currently tiled in the grid, in reading order; restored when the workspace opens.</summary>
    public List<string> OpenSessions { get; set; } = new();

    /// <summary>Sidebar folder collapsed state; the chat list hides while set.</summary>
    public bool Collapsed { get; set; }
}
