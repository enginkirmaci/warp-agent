using System.IO;
using System.Text.Json;
using Serilog;

namespace WarpAgent.Services;

/// <summary>
/// Client-side UI state (workspaces, pins, last selection) — the opencode server keeps
/// sessions but knows nothing about workspaces, pins or which chat was open. One small
/// JSON file under ~/.warp-agent.
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

    /// <summary>Last dragged sidebar width in DIPs; null keeps the 250 default.</summary>
    public double? SidebarWidth { get; set; }

    /// <summary>Titlebar sidebar toggle; null/true = visible.</summary>
    public bool? SidebarVisible { get; set; }

    private static string FilePath => AppPaths.UiStateFile;

    public static UiState Load()
    {
        AppPaths.MigrateLegacyFile("ui.json");
        try
        {
            if (File.Exists(FilePath))
                return JsonSerializer.Deserialize<UiState>(File.ReadAllText(FilePath)) ?? new UiState();
        }
        catch (Exception ex)
        {
            // corrupt or unreadable state must never block the app from starting
            Log.Warning(ex, "ui.json unreadable; starting with empty state");
        }
        return new UiState();
    }

    // Save() runs per state change (focus, send, idle); don't rebuild options each time
    private static readonly JsonSerializerOptions PrettyJson = new() { WriteIndented = true };

    public void Save()
    {
        try
        {
            Directory.CreateDirectory(AppPaths.ConfigDir);
            File.WriteAllText(FilePath, JsonSerializer.Serialize(this, PrettyJson));
        }
        catch (Exception ex)
        {
            // a failed state write is not worth interrupting the chat
            Log.Warning(ex, "ui.json write failed");
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

    /// <summary>Sidebar folder collapsed state; forced to collapsed on every launch —
    /// sections start closed and expanding one connects its server.</summary>
    public bool Collapsed { get; set; }
}
