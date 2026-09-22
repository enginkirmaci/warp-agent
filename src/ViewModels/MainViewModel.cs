using System.Collections.ObjectModel;
using System.Collections.Specialized;
using System.IO;
using System.Text;
using System.Text.Json;
using Avalonia.Threading;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using Serilog;
using WarpAgent.Models;
using WarpAgent.Services;

namespace WarpAgent.ViewModels;

public partial class MainViewModel : ObservableObject
{
    public const string DefaultModelEntry = "(server default)";
    public const string DefaultVariantEntry = "(default)";
    public const string DefaultAgentEntry = "(default build)";
    public const string DraftTitle = "New chat";

    private readonly UiState _state;
    private readonly AppSettings _settings;
    // one serve instance per workspace; several can run at the same time
    private readonly Dictionary<string, WorkspaceRuntime> _runtimes = new();
    private readonly Dictionary<string, string> _workspaceBySession = new();
    private readonly Dictionary<string, string> _titleBySession = new();
    private readonly Dictionary<string, List<PermissionItem>> _pendingBySession = new();
    private readonly Dictionary<string, List<QuestionItem>> _pendingQuestionsBySession = new();
    private readonly Dictionary<string, ChatTileViewModel> _tileBySession = new();
    // when the running turn's prompt was sent; drives the done-in note on completion
    private readonly Dictionary<string, DateTime> _turnStartBySession = new();

    /// <summary>Sessions whose current turn the user aborted; the next idle drops a "stopped in" note instead of "done in".</summary>
    private readonly HashSet<string> _userStoppedBySession = new();
    private readonly List<QueueEntry> _queue = [];
    private int _workspaceGeneration;
    private Task? _switchTask;
    private readonly HashSet<string> _runningBySession = new();
    private Dictionary<string, List<string>> _variantsByModel = new();
    private Dictionary<string, int> _contextLimitByModel = new(StringComparer.OrdinalIgnoreCase);

    private sealed class WorkspaceRuntime
    {
        public required OpenCodeServer Server { get; init; }
        public required OpenCodeApiClient Api { get; init; }
        public CancellationTokenSource? Events { get; set; }
    }

    private sealed record QueueEntry(string SessionId, string Text, List<FileAttachment> Attachments, QueuedMessageItem Item);

    /// <summary>A subagent (child) session seen on the event stream: the chat that spawned it and which agent runs there.</summary>
    private sealed record SubagentInfo(string ParentSessionId, string Agent);

    private readonly Dictionary<string, SubagentInfo> _subagentBySession = new();
    private readonly HashSet<string> _runningSubagents = new();

    /// <summary>The grid is shared: it holds open chats of every workspace and selecting
    /// a workspace never clears it.</summary>
    public ObservableCollection<ChatTileViewModel> OpenTiles { get; } = [];
    public ObservableCollection<WorkspaceItem> Workspaces { get; } = [];
    public ObservableCollection<SessionItem> Sessions { get; } = [];
    public ObservableCollection<string> Models { get; } = [DefaultModelEntry];
    public ObservableCollection<string> Agents { get; } = [DefaultAgentEntry];
    public ObservableCollection<RunningAgentItem> RunningAgents { get; } = [];

    /// <summary>Grid grows toward a square: columns = ceil(sqrt(count)) — 1→1×1, 2→2×1, 3–4→2×2, 5–6→3×2, 7–9→3×3.</summary>
    public int TileColumns => Math.Max(1, (int)Math.Ceiling(Math.Sqrt(OpenTiles.Count)));
    public bool HasTiles => OpenTiles.Count > 0;
    public bool HasNoWorkspace => Workspaces.Count == 0;
    public bool HasIdleWorkspace => Workspaces.Count > 0 && !IsServerRunning;
    public bool HasRunningWorkspace => Workspaces.Count > 0 && IsServerRunning;
    public bool CanStartSelected => SelectedWorkspace is not null && !IsServerRunning;
    public bool CanStopSelected => IsServerRunning;
    public bool MultipleWorkspaces => Workspaces.Count > 1;
    public bool HasAgents => Agents.Count > 1;
    public bool HasBackgroundAgents => RunningAgents.Any(r => r.IsBackground);

    /// <summary>"N running" summary for the flyout's Agents header (N counts background agents only).</summary>
    public string AgentsSummary
    {
        get
        {
            var n = RunningAgents.Count(r => r.IsBackground);
            return n == 1 ? "1 running" : $"{n} running";
        }
    }
    public bool HasSidePanel => HasBackgroundAgents || (FocusedTile?.HasTasks ?? false);
    public bool HasFocusedTasks => FocusedTile?.HasTasks ?? false;
    public bool HasTasksAndAgents => HasBackgroundAgents && HasFocusedTasks;
    public bool HasAttentionNeeded => RunningAgents.Any(r => r.NeedsAttention);

    public bool AnyBusy => _runningBySession.Count > 0;
    public string WorkingLabel => AnyBusy ? $"{_runningBySession.Count} working" : "working";

    internal bool IsBusy(string? sessionId) => sessionId is { } id && _runningBySession.Contains(id);

    // ---- prompt history (newest last; shared by every composer) ----

    private readonly List<string> _promptHistory = [];

    internal int PromptHistoryCount => _promptHistory.Count;

    internal string PeekPromptHistory(int stepsFromEnd) => _promptHistory[^ (stepsFromEnd + 1)];

    private void RecordPrompt(string text)
    {
        if (string.IsNullOrWhiteSpace(text))
            return;
        _promptHistory.Remove(text);
        _promptHistory.Add(text);
        if (_promptHistory.Count > 100)
            _promptHistory.RemoveRange(0, _promptHistory.Count - 100);
    }

    internal string SeedModel => _state.Model ?? _settings.DefaultModel ?? DefaultModelEntry;
    internal string SeedVariant => _settings.DefaultVariant ?? DefaultVariantEntry;
    internal string SeedAgent => _state.Agent ?? _settings.DefaultAgent ?? DefaultAgentEntry;

    public double? SidebarWidth => _state.SidebarWidth;

    /// <summary>Persists the dragged sidebar width; clamped to the same bounds as the view.</summary>
    public void SetSidebarWidth(double width)
    {
        width = Math.Clamp(width, 180, 480);
        if (Math.Abs(_state.SidebarWidth.GetValueOrDefault(250) - width) < 1)
            return;
        _state.SidebarWidth = width;
        _state.Save();
    }

    /// <summary>Edited in place by the settings page; Save persists and re-applies.</summary>
    public AppSettings Settings => _settings;

    internal bool TryGetVariants(string model, out IReadOnlyList<string> variants)
    {
        if (_variantsByModel.TryGetValue(model, out var found))
        {
            variants = found;
            return true;
        }
        variants = [];
        return false;
    }

    internal bool TryGetContextLimit(string model, out int limit) =>
        _contextLimitByModel.TryGetValue(model, out limit);

    private ChatTileViewModel? _focusedTile;
    private INotifyCollectionChanged? _focusedTasksHook;

    /// <summary>The tile that header actions act on; its tasks drive the corner flyout.</summary>
    public ChatTileViewModel? FocusedTile
    {
        get => _focusedTile;
        private set
        {
            if (_focusedTasksHook is { } oldHook)
            {
                oldHook.CollectionChanged -= OnFocusedTasksChanged;
                _focusedTasksHook = null;
            }
            if (SetProperty(ref _focusedTile, value))
            {
                OnPropertyChanged(nameof(HasSidePanel));
                OnPropertyChanged(nameof(HasTasksAndAgents));
            }
            if (_focusedTile is { } tile)
            {
                _focusedTasksHook = tile.Tasks;
                tile.Tasks.CollectionChanged += OnFocusedTasksChanged;
            }
            UpdateSidebarSelection();
        }
    }

    private void OnFocusedTasksChanged(object? sender, NotifyCollectionChangedEventArgs e)
    {
        // todos can land on an already-focused tile; the corner flyout gates on them
        OnPropertyChanged(nameof(HasSidePanel));
        OnPropertyChanged(nameof(HasTasksAndAgents));
    }

    [ObservableProperty] private WorkspaceItem? _selectedWorkspace;
    [ObservableProperty] private string _statusText = "Server stopped";
    [ObservableProperty] private bool _isServerRunning;
    [ObservableProperty] private bool _autoAllow;
    [ObservableProperty] private bool _isSettingsOpen;

    /// <summary>True from launch while the last session left tiled chats and the grid is
    /// still empty; the banner over the grid offers to restore or ignore them.</summary>
    [ObservableProperty] private bool _showRestorePrompt;

    /// <summary>Fixed at launch from the saved open-tile lists.</summary>
    public string RestorePromptText { get; private set; } = "";

    public SettingsPageViewModel SettingsPage { get; }

    /// <summary>Safety-net error overlay; CrashGuard pushes every caught exception here.</summary>
    public ErrorOverlayViewModel Error { get; } = new();

    [RelayCommand]
    private void OpenSettings() => IsSettingsOpen = true;

    [RelayCommand]
    private void CloseSettings() => IsSettingsOpen = false;

    partial void OnAutoAllowChanged(bool value)
    {
        _settings.AutoAllowAlways = value;
        _state.AutoAllowAlways = value;
        SaveState();
        _settings.Save();
    }

    /// <summary>Called after the settings page saves; pushes persisted values back into live state.</summary>
    public void ApplySavedSettings()
    {
        if (_settings.AutoAllowAlways is { } allowed && allowed != AutoAllow)
            AutoAllow = allowed;
        FontScale.Apply(_settings.FontScale);
        AccentTheme.Apply(_settings.Accent);
    }

    partial void OnIsServerRunningChanged(bool value)
    {
        OnPropertyChanged(nameof(CanStartSelected));
        OnPropertyChanged(nameof(CanStopSelected));
        OnPropertyChanged(nameof(HasIdleWorkspace));
        OnPropertyChanged(nameof(HasRunningWorkspace));
        RefreshTileConnectivity();
    }

    partial void OnSelectedWorkspaceChanged(WorkspaceItem? value)
    {
        OnPropertyChanged(nameof(CanStartSelected));
        OnPropertyChanged(nameof(CanStopSelected));
        UpdateSidebarSelection();
        if (value is null)
        {
            ClearTiles();
            Sessions.Clear();
            return;
        }
        _switchTask = SelectWorkspaceAsync(value);
    }

    /// <summary>Opens the clicked sidebar chat: focuses its tile when one exists, otherwise
    /// loads it from its own workspace's server — without switching the selected
    /// workspace, so chats of different workspaces share one grid.</summary>
    public async Task SelectSessionAsync(SessionItem session)
    {
        if (session.Id == FocusedTile?.SessionId)
            return;
        if (TileFor(session.Id) is { } existing)
        {
            FocusTile(existing);
            return;
        }
        if (!_workspaceBySession.TryGetValue(session.Id, out var path) ||
            Workspaces.FirstOrDefault(w => w.FolderPath == path) is not { } owner)
        {
            SnapSelection();
            return;
        }
        await OpenSessionIntoGridAsync(owner, session.Id);
    }

    public void SelectWorkspace(WorkspaceItem ws)
    {
        if (!ReferenceEquals(SelectedWorkspace, ws))
            SelectedWorkspace = ws; // the change hook stores its switch task in _switchTask
    }

    /// <summary>Expanding a workspace's chat list shows intent to work in it: connect its
    /// server so the chat list fills; an already-running workspace is left alone.</summary>
    public void ConnectExpandedWorkspace(WorkspaceItem ws)
    {
        if (!IsServerUp(ws))
            _ = StartWorkspaceCommand.ExecuteAsync(ws);
    }

    private void OnSidePanelChanged()
    {
        OnPropertyChanged(nameof(HasBackgroundAgents));
        OnPropertyChanged(nameof(AgentsSummary));
        OnPropertyChanged(nameof(HasSidePanel));
        OnPropertyChanged(nameof(HasTasksAndAgents));
        OnPropertyChanged(nameof(HasAttentionNeeded));
    }

    public MainViewModel()
    {
        _state = UiState.Load();
        _settings = AppSettings.Load();
        // one-time migration: the pre-settings auto-allow choice moves into settings.json
        _settings.AutoAllowAlways ??= _state.AutoAllowAlways;
        FontScale.Apply(_settings.FontScale);
        AccentTheme.Apply(_settings.Accent);
        SettingsPage = new SettingsPageViewModel(this);
        SettingsPage.Saved += (_, _) => IsSettingsOpen = false;
        if (_state.Workspaces.Count == 0 && !string.IsNullOrWhiteSpace(_state.Folder))
            _state.Workspaces.Add(new WorkspaceState { Path = NormalizeDir(_state.Folder) });
        EnsureGeneralWorkspace();
        // every launch starts with the workspace sections collapsed, whatever was saved
        foreach (var ws in _state.Workspaces)
        {
            ws.Collapsed = true;
            // saved open-tile ids of chats that no longer exist could never restore
            ws.OpenSessions.RemoveAll(id => !ws.Sessions.Contains(id));
            var item = new WorkspaceItem(ws) { StateChanged = SaveState };
            Workspaces.Add(item);
            foreach (var sid in ws.Sessions)
                _workspaceBySession[sid] = ws.Path;
        }
        var pendingTiles = Workspaces.Sum(w => w.State.OpenSessions.Count);
        RestorePromptText = pendingTiles == 1 ? "1 chat was open last time" : $"{pendingTiles} chats were open last time";
        ShowRestorePrompt = pendingTiles > 0;
        _autoAllow = _state.AutoAllowAlways ?? true;
        Sessions.CollectionChanged += (_, _) => SyncWorkspaceChats();
        Workspaces.CollectionChanged += (_, _) =>
        {
            OnPropertyChanged(nameof(HasNoWorkspace));
            OnPropertyChanged(nameof(HasIdleWorkspace));
            OnPropertyChanged(nameof(HasRunningWorkspace));
            OnPropertyChanged(nameof(CanStartSelected));
            OnPropertyChanged(nameof(MultipleWorkspaces));
            UpdateSidebarSelection();
        };
        RunningAgents.CollectionChanged += (_, _) => OnSidePanelChanged();
        Agents.CollectionChanged += (_, _) => OnPropertyChanged(nameof(HasAgents));
        OpenTiles.CollectionChanged += (_, _) =>
        {
            OnPropertyChanged(nameof(TileColumns));
            OnPropertyChanged(nameof(HasTiles));
            RefreshBackgroundFlags();
            // a tile reaching the grid on its own (manual open, expand-restore, new chat)
            // ends the relaunch offer without touching the saved lists
            if (OpenTiles.Count > 0)
                ShowRestorePrompt = false;
        };
        // restore the last workspace selection but never auto-start a server
        var selected = Workspaces.FirstOrDefault(w => w.FolderPath == _state.ActiveWorkspace) ?? Workspaces.FirstOrDefault();
        if (selected is not null)
            SelectedWorkspace = selected;
    }

    /// <summary>General is built in: general-purpose chats that belong to no project folder.
    /// Its state (sessions, open tiles) persists like any workspace, it leads the sidebar
    /// list, and it cannot be removed. The server still needs a cwd; ~/.warp-agent/general
    /// is that directory and only shows up as the section's tooltip.</summary>
    private void EnsureGeneralWorkspace()
    {
        var general = _state.Workspaces.FirstOrDefault(w => w.Path == AppPaths.GeneralWorkspacePath);
        if (general is null)
        {
            general = new WorkspaceState { Path = AppPaths.GeneralWorkspacePath };
            _state.Workspaces.Insert(0, general);
        }
        else if (_state.Workspaces[0] != general)
        {
            _state.Workspaces.Remove(general);
            _state.Workspaces.Insert(0, general);
        }
        try
        {
            Directory.CreateDirectory(AppPaths.GeneralWorkspacePath);
        }
        catch (Exception ex)
        {
            // the server start below surfaces a bad folder on its own
            Log.Warning(ex, "could not create the general workspace folder");
        }
    }

    private void SaveState()
    {
        _state.ActiveWorkspace = SelectedWorkspace?.FolderPath;
        _state.AutoAllowAlways = AutoAllow;
        PersistTiles();
        _state.Save();
    }

    /// <summary>Writes every running workspace's tiled chats; a stopped workspace keeps
    /// its saved list untouched.</summary>
    private void PersistTiles()
    {
        foreach (var ws in Workspaces)
        {
            if (!_runtimes.ContainsKey(ws.FolderPath))
                continue;
            var ids = OpenTiles
                .Where(t => t.SessionId is { } sid && _workspaceBySession.TryGetValue(sid, out var path) && path == ws.FolderPath)
                .Select(t => t.SessionId!)
                .ToList();
            ws.State.OpenSessions = ids;
            if (ReferenceEquals(ws, SelectedWorkspace))
                ws.State.ActiveSession = FocusedTile?.SessionId is { } focus && ids.Contains(focus) ? focus : null;
        }
    }

    internal void PersistComposer(ChatTileViewModel tile)
    {
        _state.Model = tile.SelectedModel is { } m && m != DefaultModelEntry ? m : null;
        _state.Variant = tile.SelectedVariant is { } v && v != DefaultVariantEntry ? v : null;
        _state.Agent = tile.SelectedAgent is { } a && a != DefaultAgentEntry ? a : null;
        _state.Save();
    }

    private void NotifyBusyChanged()
    {
        OnPropertyChanged(nameof(AnyBusy));
        OnPropertyChanged(nameof(WorkingLabel));
        foreach (var tile in OpenTiles)
            tile.RefreshBusy();
    }

    private OpenCodeApiClient? SelectedApi =>
        SelectedWorkspace is { } ws && _runtimes.TryGetValue(ws.FolderPath, out var rt) ? rt.Api : null;

    private OpenCodeApiClient? ApiFor(string sessionId) =>
        EffectiveWorkspacePath(sessionId) is { } path && _runtimes.TryGetValue(path, out var rt) ? rt.Api : null;

    /// <summary>A subagent session lives on the server of the chat that spawned it.</summary>
    private string? EffectiveWorkspacePath(string sessionId) =>
        _workspaceBySession.TryGetValue(sessionId, out var path) ? path
        : _subagentBySession.TryGetValue(sessionId, out var sub) && _workspaceBySession.TryGetValue(sub.ParentSessionId, out var parentPath)
            ? parentPath
            : null;

    private string WorkspaceFor(string sessionId) =>
        EffectiveWorkspacePath(sessionId) is { } path && Workspaces.FirstOrDefault(w => w.FolderPath == path) is { } ws
            ? ws.Name
            : "";

    private bool IsServerUp(WorkspaceItem ws) =>
        _runtimes.TryGetValue(ws.FolderPath, out var rt) && rt.Server.IsRunning;

    private void DropSubagentRows(string folderPath)
    {
        foreach (var childId in _runningSubagents
                     .Where(id => EffectiveWorkspacePath(id) == folderPath)
                     .ToList())
        {
            SetSubagentRunning(childId, false);
            // the mapping only describes live runs; keep it from growing forever
            _subagentBySession.Remove(childId);
        }
    }

    internal bool HasRuntime(string sessionId) => ApiFor(sessionId) is not null;

    private void RefreshTileConnectivity()
    {
        foreach (var tile in OpenTiles)
            tile.RefreshConnectivity();
    }

    private bool IsAppSession(string sessionId) => _workspaceBySession.ContainsKey(sessionId);

    private string TitleFor(string sessionId) =>
        _titleBySession.TryGetValue(sessionId, out var title)
            ? title
            : $"agent {sessionId[..Math.Min(8, sessionId.Length)]}";

    internal ChatTileViewModel? TileFor(string? sessionId) =>
        sessionId is { } id && _tileBySession.TryGetValue(id, out var tile) ? tile : null;

    private ChatTileViewModel? TileFromData(JsonElement data, WorkspaceItem ws)
    {
        if (ReadSessionId(data) is { } id)
            return TileFor(id);
        // events without a session id belong to a tile of the same workspace
        return FocusedTile is { } focused && focused.SessionId is { } sid &&
               _workspaceBySession.TryGetValue(sid, out var path) && path == ws.FolderPath
            ? focused
            : null;
    }

    // ---- tile grid: open, close, focus ----

    public void FocusTile(ChatTileViewModel? tile)
    {
        if (ReferenceEquals(FocusedTile, tile))
            return;
        if (FocusedTile is { } old)
            old.IsFocused = false;
        FocusedTile = tile;
        // record the focused chat only on the workspace it belongs to
        if (SelectedWorkspace is { } ws &&
            (tile?.SessionId is not { } sid || (_workspaceBySession.TryGetValue(sid, out var owner) && owner == ws.FolderPath)))
            ws.State.ActiveSession = tile?.SessionId;
        if (tile is { } focused)
            focused.IsFocused = true;
        SnapSelection();
        PersistTiles();
        SaveState();
    }

    /// <summary>Closing a tile only un-tiles it; the chat stays in the sidebar and keeps running in the background.</summary>
    public void CloseTile(ChatTileViewModel tile)
    {
        var wasFocused = ReferenceEquals(FocusedTile, tile);
        DetachTile(tile);
        if (!wasFocused)
        {
            PersistTiles();
            SaveState();
        }
    }

    private void DetachTile(ChatTileViewModel tile)
    {
        tile.DetachFromOwner();
        OpenTiles.Remove(tile);
        if (tile.SessionId is { } id)
        {
            _tileBySession.Remove(id);
            ForgetSavedTile(id);
        }
        if (!ReferenceEquals(FocusedTile, tile))
            return;
        FocusTile(OpenTiles.LastOrDefault());
    }

    /// <summary>A closed chat leaves the saved open list too, even while its workspace server
    /// is stopped — PersistTiles would keep the stale id and offer it again next launch.</summary>
    private void ForgetSavedTile(string sessionId)
    {
        if (!_workspaceBySession.TryGetValue(sessionId, out var path) ||
            Workspaces.FirstOrDefault(w => w.FolderPath == path) is not { } ws)
            return;
        ws.State.OpenSessions.Remove(sessionId);
        if (ws.State.ActiveSession == sessionId)
            ws.State.ActiveSession = null;
    }

    private void DetachTile(string sessionId)
    {
        if (TileFor(sessionId) is { } tile)
            DetachTile(tile);
    }

    private void ClearTiles()
    {
        foreach (var tile in OpenTiles)
        {
            tile.IsFocused = false;
            tile.DetachFromOwner();
        }
        OpenTiles.Clear();
        _tileBySession.Clear();
        FocusedTile = null;
    }

    /// <summary>Drafts without content are dropped once real chats are restored.</summary>
    private void PruneUntouchedDrafts()
    {
        if (_tileBySession.Count == 0)
            return;
        foreach (var tile in OpenTiles.Where(t => t.IsUntouched).ToList())
        {
            if (OpenTiles.Count == 1)
                break;
            tile.DetachFromOwner();
            OpenTiles.Remove(tile);
            if (ReferenceEquals(FocusedTile, tile))
                FocusedTile = null;
        }
    }

    private void ShowQueuedFor(ChatTileViewModel tile)
    {
        if (tile.SessionId is not { } id)
            return;
        foreach (var entry in _queue.Where(e => e.SessionId == id).ToList())
            tile.QueueRows.Add(entry.Item);
    }

    [RelayCommand]
    private async Task NewChatAsync()
    {
        if (SelectedWorkspace is null)
            return;
        SelectedWorkspace.IsExpanded = true; // the new chat belongs in a visible sidebar section
        if (OpenTiles.LastOrDefault() is { } last && last.IsUntouched)
        {
            FocusTile(last);
        }
        else
        {
            var tile = new ChatTileViewModel(this);
            OpenTiles.Add(tile);
            FocusTile(tile);
        }
        // a new chat is an explicit intent to talk: connect the server if it isn't up
        if (!IsServerUp(SelectedWorkspace))
            _ = StartWorkspaceCommand.ExecuteAsync(SelectedWorkspace);
    }

    [RelayCommand]
    private async Task NewChatForWorkspaceAsync(WorkspaceItem? ws)
    {
        if (ws is null)
            return;
        if (!ReferenceEquals(ws, SelectedWorkspace))
        {
            SelectedWorkspace = ws;
            if (_switchTask is { } task)
            {
                try
                {
                    await task;
                }
                catch
                {
                    // switch failures surface through notes elsewhere
                }
            }
        }
        await NewChatAsync();
    }

    // ---- workspaces ----

    public void AddWorkspace(string path)
    {
        if (string.IsNullOrWhiteSpace(path))
            return;
        var normalized = NormalizeDir(path);
        if (!Directory.Exists(normalized))
        {
            Announce($"Folder does not exist: {normalized}", error: true);
            return;
        }
        if (Workspaces.Any(w => w.FolderPath == normalized))
        {
            Announce("That folder is already a workspace.");
            return;
        }
        var state = new WorkspaceState { Path = normalized };
        _state.Workspaces.Add(state);
        var item = new WorkspaceItem(state) { StateChanged = SaveState };
        Workspaces.Add(item);
        SaveState();
        SelectedWorkspace = item;
    }

    public void RemoveWorkspace(WorkspaceItem ws)
    {
        if (ws.IsGeneral)
            return; // built in, always there
        var wasSelected = ReferenceEquals(SelectedWorkspace, ws);
        if (_runtimes.TryGetValue(ws.FolderPath, out var rt))
        {
            rt.Events?.Cancel();
            rt.Server.Stop();
            _runtimes.Remove(ws.FolderPath);
        }
        DropSubagentRows(ws.FolderPath);
        foreach (var sid in ws.State.Sessions)
            SetRunning(sid, false);
        DropQueue(ws.State.Sessions);
        DropPending(ws.State.Sessions);
        foreach (var sid in ws.State.Sessions)
        {
            _workspaceBySession.Remove(sid);
            _titleBySession.Remove(sid);
            _turnStartBySession.Remove(sid);
            _userStoppedBySession.Remove(sid);
        }
        // only this workspace's tiles leave the grid; other workspaces' stay
        foreach (var tile in OpenTiles
                     .Where(t => t.SessionId is { } sid && ws.State.Sessions.Contains(sid))
                     .ToList())
            DetachTile(tile);
        _state.Workspaces.Remove(ws.State);
        Workspaces.Remove(ws);
        if (wasSelected)
        {
            // hand selection to the next workspace; null only when the last one is gone
            SelectedWorkspace = Workspaces.FirstOrDefault();
            if (SelectedWorkspace is null)
            {
                IsServerRunning = false;
                StatusText = "Server stopped";
            }
        }
        RefreshTileConnectivity();
        SaveState();
    }

    [RelayCommand]
    private async Task StartWorkspaceAsync(WorkspaceItem? ws)
    {
        if (ws is null || (_runtimes.TryGetValue(ws.FolderPath, out var existing) && existing.Server.IsRunning))
            return;
        StatusText = $"Starting opencode serve — {ws.Name}…";
        var serverLog = Log.ForContext("SourceContext", $"opencode/{ws.Name}");
        try
        {
            var port = OpenCodeServer.FindFreePort();
            var server = new OpenCodeServer();
            await server.StartAsync(_settings.OpenCodeExecutable, ws.FolderPath, port, line => serverLog.Information("{Line}", line), CancellationToken.None);
            var rt = new WorkspaceRuntime { Server = server, Api = new OpenCodeApiClient(server.BaseUrl) };
            _runtimes[ws.FolderPath] = rt;
            ws.IsRunning = true;
            RefreshTileConnectivity();
            rt.Events = new CancellationTokenSource();
            _ = PumpEventsAsync(rt.Api, rt.Events.Token, ws);
            var gen = ++_workspaceGeneration;
            _ = LoadModelsAsync(rt, gen);
            _ = LoadAgentsAsync(rt, gen);
            if (!ReferenceEquals(ws, SelectedWorkspace))
            {
                // started from the context menu; the grid keeps whatever is tiled
                StatusText = $"Server ready — {ws.Name} ({server.BaseUrl})";
                _ = PrimeWorkspaceTitlesAsync(ws, rt);
                _ = LoadWorkspaceChatsAsync(ws, rt);
                return;
            }
            IsServerRunning = true;
            StatusText = (server.Spawned ? "Server ready — " : "Attached — ") + server.BaseUrl;
            await RefreshSessionsAsync();
            // restore this workspace's saved tiles; sessions already tiled are skipped
            await RestoreOpenTilesAsync(ws, rt, gen, ws.State.OpenSessions.ToList());
        }
        catch (Exception ex)
        {
            Log.Error(ex, "server start failed for {Workspace}", ws.Name);
            StatusText = "Start failed";
            AddNote($"Failed to start: {ex.Message}", error: true);
        }
    }

    [RelayCommand]
    private void StopWorkspace(WorkspaceItem? ws)
    {
        if (ws is null || !_runtimes.TryGetValue(ws.FolderPath, out var rt))
            return;
        rt.Events?.Cancel();
        var wasAttached = !rt.Server.Spawned;
        rt.Server.Stop();
        _runtimes.Remove(ws.FolderPath);
        DropSubagentRows(ws.FolderPath);
        ws.IsRunning = false;
        ws.Chats.Clear();
        RefreshTileConnectivity();
        var sessionIds = ws.State.Sessions.ToList();
        foreach (var sid in sessionIds)
            SetRunning(sid, false);
        DropQueue(sessionIds);
        DropPending(sessionIds);
        if (ReferenceEquals(ws, SelectedWorkspace))
        {
            IsServerRunning = false;
            StatusText = "Server stopped";
            Sessions.Clear();
            SnapSelection();
        }
        if (wasAttached)
            AddNote("Disconnected (the attached server is still running).");
    }

    public void Shutdown()
    {
        foreach (var rt in _runtimes.Values)
        {
            rt.Events?.Cancel();
            rt.Server.Stop();
        }
        _runtimes.Clear();
    }

    /// <summary>Selecting a workspace retargets the sidebar, status line and new chats;
    /// the grid keeps every open tile, so different workspaces share it. An empty grid
    /// stays empty — chats open through the sidebar or the workspace's + button.</summary>
    private async Task SelectWorkspaceAsync(WorkspaceItem ws)
    {
        Sessions.Clear();
        if (!_runtimes.TryGetValue(ws.FolderPath, out var rt) || !rt.Server.IsRunning)
        {
            IsServerRunning = false;
            StatusText = $"Server stopped — {ws.Name}";
            return;
        }
        IsServerRunning = true;
        StatusText = (rt.Server.Spawned ? "Server ready — " : "Attached — ") + rt.Server.BaseUrl;
        var gen = ++_workspaceGeneration;
        _ = LoadModelsAsync(rt, gen);
        _ = LoadAgentsAsync(rt, gen);
        await RefreshSessionsAsync();
        if (OpenTiles.Count == 0)
            await RestoreOpenTilesAsync(ws, rt, gen, ws.State.OpenSessions.ToList());
    }

    /// <summary>Refills the grid from a workspace's saved open chats; sessions already tiled
    /// are skipped, and focus only moves when something was actually added.</summary>
    private async Task RestoreOpenTilesAsync(WorkspaceItem ws, WorkspaceRuntime rt, int gen, List<string> wanted)
    {
        var added = 0;
        foreach (var id in wanted)
        {
            if (!Sessions.Any(s => s.Id == id) || _tileBySession.ContainsKey(id))
                continue;
            try
            {
                var messages = await rt.Api.GetMessagesAsync(id, CancellationToken.None);
                if (gen != _workspaceGeneration || !ReferenceEquals(SelectedWorkspace, ws))
                    return;
                var tile = new ChatTileViewModel(this);
                tile.SetSession(id, TitleFor(id));
                _tileBySession[id] = tile;
                OpenTiles.Add(tile);
                tile.Restore(messages);
                FlushPendingPermissions(id);
                FlushPendingQuestions(id);
                _ = PullPendingQuestionsAsync(id, rt.Api);
                ShowQueuedFor(tile);
                added++;
            }
            catch (Exception ex)
            {
                if (gen != _workspaceGeneration || !ReferenceEquals(SelectedWorkspace, ws))
                    return;
                Log.Warning(ex, "could not load chat {Session}", id);
                AddNote($"Could not load chat: {ex.Message}", error: true);
            }
        }
        if (gen != _workspaceGeneration || !ReferenceEquals(SelectedWorkspace, ws))
            return;
        PruneUntouchedDrafts();
        if (added == 0 || OpenTiles.Count == 0)
            return;
        var focus = ws.State.ActiveSession is { } active && TileFor(active) is { } activeTile
            ? activeTile
            : OpenTiles.Last();
        FocusTile(focus);
    }

    /// <summary>Reopens every chat the last session left tiled, starting each involved
    /// workspace's server; RestoreTilesForWorkspaceAsync skips already-tiled chats, so
    /// the selected workspace's own start-path restore never doubles anything.</summary>
    [RelayCommand]
    private async Task RestorePreviousSessionAsync()
    {
        ShowRestorePrompt = false;
        foreach (var ws in Workspaces.Where(w => w.State.OpenSessions.Count > 0).ToList())
        {
            ws.IsExpanded = true; // restored chats belong in a visible sidebar section
            var wanted = ws.State.OpenSessions.ToList();
            if (!IsServerUp(ws))
                await StartWorkspaceCommand.ExecuteAsync(ws);
            if (!_runtimes.TryGetValue(ws.FolderPath, out var rt) || !rt.Server.IsRunning)
                continue; // start failed; its note is already on screen
            await RestoreTilesForWorkspaceAsync(ws, rt, wanted);
        }
        if (FocusedTile is null && OpenTiles.LastOrDefault() is { } last)
            FocusTile(last);
    }

    /// <summary>Ignore means ignored: the saved open lists are dropped, so the offer
    /// only comes back once new tiles are opened and closed.</summary>
    [RelayCommand]
    private void DismissRestorePrompt()
    {
        ShowRestorePrompt = false;
        foreach (var ws in Workspaces)
            ws.State.OpenSessions.Clear();
        SaveState();
    }

    /// <summary>Restores saved tiles for a workspace started without switching selection;
    /// same intake as RestoreOpenTilesAsync but keyed to this workspace's own chat list.</summary>
    private async Task RestoreTilesForWorkspaceAsync(WorkspaceItem ws, WorkspaceRuntime rt, List<string> wanted)
    {
        // titles would otherwise flash fallback names until the background chat load lands
        try
        {
            var known = ws.State.Sessions.ToHashSet();
            foreach (var s in await rt.Api.ListSessionsAsync(CancellationToken.None))
                if (known.Contains(s.Id))
                    _titleBySession[s.Id] = s.Title;
        }
        catch (Exception ex)
        {
            Log.Debug(ex, "title prime failed for {Workspace}", ws.Name);
        }
        foreach (var id in wanted)
        {
            if (!ws.State.Sessions.Contains(id) || _tileBySession.ContainsKey(id))
                continue;
            try
            {
                var messages = await rt.Api.GetMessagesAsync(id, CancellationToken.None);
                if (!_runtimes.TryGetValue(ws.FolderPath, out var live) || !ReferenceEquals(live, rt))
                    return; // the workspace was stopped or removed mid-restore
                if (_tileBySession.ContainsKey(id))
                    continue;
                var tile = new ChatTileViewModel(this);
                tile.SetSession(id, TitleFor(id));
                _tileBySession[id] = tile;
                OpenTiles.Add(tile);
                tile.Restore(messages);
                FlushPendingPermissions(id);
                FlushPendingQuestions(id);
                _ = PullPendingQuestionsAsync(id, rt.Api);
                ShowQueuedFor(tile);
            }
            catch (Exception ex)
            {
                if (!_runtimes.TryGetValue(ws.FolderPath, out var live) || !ReferenceEquals(live, rt))
                    return;
                Log.Warning(ex, "could not load chat {Session}", id);
                AddNote($"Could not load chat: {ex.Message}", error: true);
            }
        }
    }

    // ---- chats ----

    [RelayCommand]
    private Task OpenSessionAsync(SessionItem? session) =>
        session is null ? Task.CompletedTask : SelectSessionAsync(session);

    /// <summary>Loads a chat into a grid tile through its own workspace's server, starting
    /// that server if needed; the selected workspace is left alone.</summary>
    private async Task OpenSessionIntoGridAsync(WorkspaceItem ws, string sessionId)
    {
        ws.IsExpanded = true; // the opened chat's row belongs in a visible sidebar section
        if (!_runtimes.TryGetValue(ws.FolderPath, out var rt))
        {
            await StartWorkspaceCommand.ExecuteAsync(ws);
            if (!_runtimes.TryGetValue(ws.FolderPath, out rt))
                return; // start failed; its note is already on screen
        }
        try
        {
            var messages = await rt.Api.GetMessagesAsync(sessionId, CancellationToken.None);
            if (TileFor(sessionId) is { } existing)
            {
                FocusTile(existing);
                return;
            }
            if (!_runtimes.ContainsKey(ws.FolderPath))
                return; // workspace removed while loading
            var tile = new ChatTileViewModel(this);
            tile.SetSession(sessionId, TitleFor(sessionId));
            _tileBySession[sessionId] = tile;
            OpenTiles.Add(tile);
            tile.Restore(messages);
            FlushPendingPermissions(sessionId);
            FlushPendingQuestions(sessionId);
            _ = PullPendingQuestionsAsync(sessionId, rt.Api);
            ShowQueuedFor(tile);
            ws.State.ActiveSession = sessionId;
            FocusTile(tile); // also persists the open-tile lists
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "could not load chat {Session}", sessionId);
            AddNote($"Could not load chat: {ex.Message}", error: true);
            SnapSelection();
        }
    }

    /// <summary>Opens a chat from the running-agents panel straight into the grid; a
    /// subagent row opens the chat that spawned it. The selected workspace stays put.</summary>
    public async Task OpenRunningAsync(RunningAgentItem running)
    {
        var target = running.ParentSessionId ?? running.SessionId;
        if (!_workspaceBySession.TryGetValue(target, out var path) ||
            Workspaces.FirstOrDefault(w => w.FolderPath == path) is not { } ws)
            return;
        await OpenSessionIntoGridAsync(ws, target);
    }

    internal async Task SendFromTileAsync(ChatTileViewModel tile)
    {
        var text = tile.Input.Trim();
        if (text.Length == 0 && tile.Attachments.Count == 0)
            return;
        // an existing chat talks to its own workspace's server; a draft to the selected one
        if (tile.SessionId is { } id)
        {
            if (ApiFor(id) is null)
            {
                tile.AddNote("This chat's workspace server is stopped — start it to continue.", error: true);
                return;
            }
        }
        else if (!IsServerRunning || SelectedApi is null)
            return;
        RecordPrompt(text);
        tile.ResetPromptHistory();
        var attachments = tile.TakeAttachments();
        tile.Input = "";
        if (tile.IsBusy)
        {
            // park the message on this chat; it is sent when that chat goes idle
            var item = new QueuedMessageItem(text, attachments.Count)
            {
                Remove = RemoveQueued,
                Steer = queued => _ = SteerQueuedAsync(tile, queued),
                Edit = queued => EditQueued(tile, queued),
            };
            _queue.Add(new QueueEntry(tile.SessionId!, text, attachments, item));
            tile.QueueRows.Add(item);
            return;
        }
        if (tile.SessionId is null)
        {
            try
            {
                await EnsureSessionAsync(tile);
            }
            catch (Exception ex)
            {
                tile.AddNote($"Could not create a session: {ex.Message}", error: true);
                return;
            }
        }
        if (tile.SessionId is { } sessionId)
            SendCoreAsync(sessionId, text, attachments);
    }

    private void SendCoreAsync(string sessionId, string text, List<FileAttachment> attachments)
    {
        if (ApiFor(sessionId) is not { } api)
            return;
        var tile = TileFor(sessionId);
        var sentAt = DateTime.Now;
        if (tile is not null)
        {
            if (text.Length > 0)
                tile.ChatItems.Add(new UserMessageItem(text, sentAt));
            foreach (var attachment in attachments)
                tile.ChatItems.Add(attachment.IsImage
                    ? new ImageItem(attachment.FileName, attachment.Data, sentAt)
                    : new SystemNoteItem($"Attachment: {attachment.FileName}"));
        }
        SetRunning(sessionId, true);
        _turnStartBySession[sessionId] = sentAt;
        AutoTitle(sessionId, api, text.Length > 0 ? text : attachments.FirstOrDefault()?.FileName ?? DraftTitle);
        var model = tile?.SelectedModel is { } m && m != DefaultModelEntry ? m : null;
        var variant = tile?.SelectedVariant is { } v && v != DefaultVariantEntry ? v : null;
        var agent = tile?.SelectedAgent is { } a && a != DefaultAgentEntry ? a : null;
        _ = Task.Run(async () =>
        {
            try
            {
                await api.SendMessageAsync(sessionId, text, model, variant, agent, attachments, CancellationToken.None);
            }
            catch (Exception ex)
            {
                Log.Error(ex, "message send failed for {Session}", sessionId);
                PostNote(sessionId, $"Message failed: {ex.Message}", error: true);
            }
        });
    }

    /// <summary>Steer: jump the queue and interrupt the running turn so this prompt runs next.</summary>
    private async Task SteerQueuedAsync(ChatTileViewModel tile, QueuedMessageItem item)
    {
        var index = _queue.FindIndex(e => ReferenceEquals(e.Item, item));
        if (index < 0 || tile.SessionId is not { } sid)
            return;
        var entry = _queue[index];
        if (!IsBusy(sid))
        {
            // nothing running anymore: dispatch it right away
            _queue.RemoveAt(index);
            tile.QueueRows.Remove(item);
            SendCoreAsync(sid, entry.Text, entry.Attachments);
            return;
        }
        // put it in front; the running turn's idle then sends this one first
        if (index > 0)
        {
            _queue.RemoveAt(index);
            _queue.Insert(0, entry);
        }
        await AbortTileAsync(tile);
    }

    /// <summary>Edit: pull a queued prompt back into the composer, dropping it from the queue.</summary>
    private void EditQueued(ChatTileViewModel tile, QueuedMessageItem item)
    {
        var index = _queue.FindIndex(e => ReferenceEquals(e.Item, item));
        if (index < 0)
            return;
        var entry = _queue[index];
        _queue.RemoveAt(index);
        tile.QueueRows.Remove(item);
        tile.LoadIntoInput(entry.Text);
        foreach (var attachment in entry.Attachments)
            tile.AttachFile(attachment.FileName, attachment.Mime, attachment.Data);
    }

    private void RemoveQueued(QueuedMessageItem item)
    {
        var index = _queue.FindIndex(e => ReferenceEquals(e.Item, item));
        if (index >= 0)
            _queue.RemoveAt(index);
        foreach (var tile in OpenTiles)
            tile.QueueRows.Remove(item);
    }

    private void DropQueue(IReadOnlyCollection<string> sessionIds)
    {
        for (var i = _queue.Count - 1; i >= 0; i--)
        {
            if (!sessionIds.Contains(_queue[i].SessionId))
                continue;
            var entry = _queue[i];
            TileFor(entry.SessionId)?.QueueRows.Remove(entry.Item);
            _queue.RemoveAt(i);
        }
    }

    private void DropPending(IReadOnlyCollection<string> sessionIds)
    {
        foreach (var sid in sessionIds)
        {
            if (_pendingBySession.Remove(sid, out _))
                MarkAttention(sid, false);
            // parked question cards would otherwise accumulate for closed chats
            _pendingQuestionsBySession.Remove(sid, out _);
        }
    }

    /// <summary>Sends the oldest queued message after that chat finishes its turn; called on session.idle.</summary>
    private void TryDrainQueue(string sessionId)
    {
        if (ApiFor(sessionId) is null || IsBusy(sessionId))
            return; // busy: a steered prompt already took over; the rest stay queued
        var entry = _queue.FirstOrDefault(e => e.SessionId == sessionId);
        if (entry is null)
            return;
        _queue.Remove(entry);
        TileFor(sessionId)?.QueueRows.Remove(entry.Item);
        SendCoreAsync(sessionId, entry.Text, entry.Attachments);
    }

    internal async Task AbortTileAsync(ChatTileViewModel tile)
    {
        if (tile.SessionId is not { } id || !IsBusy(id) || ApiFor(id) is not { } api)
            return;
        try
        {
            await api.AbortAsync(id, CancellationToken.None);
            _userStoppedBySession.Add(id);
            SetRunning(id, false);
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "abort failed for {Session}", id);
            tile.AddNote($"Abort failed: {ex.Message}", error: true);
        }
    }

    [RelayCommand]
    private async Task StopRunningAsync(string? sessionId)
    {
        if (sessionId is null || ApiFor(sessionId) is not { } api)
            return;
        try
        {
            await api.AbortAsync(sessionId, CancellationToken.None);
            _userStoppedBySession.Add(sessionId);
            SetRunning(sessionId, false);
            SetSubagentRunning(sessionId, false);
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "abort failed for {Session}", sessionId);
            PostNote(_subagentBySession.TryGetValue(sessionId, out var sub) ? sub.ParentSessionId : sessionId,
                $"Abort failed: {ex.Message}", error: true);
        }
    }

    private async Task EnsureSessionAsync(ChatTileViewModel tile)
    {
        if (tile.SessionId is not null)
            return;
        if (SelectedWorkspace is not { } ws || !_runtimes.TryGetValue(ws.FolderPath, out var rt))
            return;
        var info = await rt.Api.CreateSessionAsync(DraftTitle, CancellationToken.None);
        _workspaceBySession[info.Id] = ws.FolderPath;
        _titleBySession[info.Id] = info.Title;
        _tileBySession[info.Id] = tile;
        tile.SetSession(info.Id, info.Title);
        RefreshBackgroundFlags();
        tile.AutoTitlePending = true;
        ws.State.Sessions.Insert(0, info.Id);
        Sessions.Insert(0, new SessionItem(info.Id, info.Title, info.UpdatedAt));
        SnapSelection();
        PersistTiles();
        SaveState();
    }

    private void AutoTitle(string sessionId, OpenCodeApiClient api, string text)
    {
        if (TileFor(sessionId) is not { } tile || !tile.AutoTitlePending)
            return;
        tile.AutoTitlePending = false;
        var title = ChatParsing.DeriveTitle(text);
        _ = Task.Run(async () =>
        {
            SessionInfo info;
            try
            {
                info = await api.RenameSessionAsync(sessionId, title, CancellationToken.None);
            }
            catch (Exception ex)
            {
                Log.Debug(ex, "auto title failed for {Session}", sessionId);
                return;
            }
            await Dispatcher.UIThread.InvokeAsync(() =>
            {
                var finalTitle = info.Title.Length > 0 ? info.Title : title;
                _titleBySession[sessionId] = finalTitle;
                TileFor(sessionId)?.SetTitle(finalTitle);
                var item = Sessions.FirstOrDefault(s => s.Id == sessionId);
                if (item is null)
                    return;
                item.Title = finalTitle;
                item.Updated = info.UpdatedAt;
                SortSessions();
            });
        });
    }

    public void TogglePin(SessionItem session)
    {
        session.IsPinned = !session.IsPinned;
        if (session.IsPinned)
            _state.Pins[session.Id] = true;
        else
            _state.Pins.Remove(session.Id);
        SaveState();
        SortSessions();
    }

    public void BeginRename(SessionItem session)
    {
        session.DraftTitle = session.Title;
        session.IsEditing = true;
    }

    public void CancelRename(SessionItem session) => session.IsEditing = false;

    [RelayCommand]
    private async Task CommitRenameAsync(SessionItem? session)
    {
        if (session is null)
            return;
        session.IsEditing = false;
        var title = session.DraftTitle.Trim();
        if (title.Length == 0 || title == session.Title || ApiFor(session.Id) is not { } api)
            return;
        try
        {
            var info = await api.RenameSessionAsync(session.Id, title, CancellationToken.None);
            session.Title = info.Title.Length > 0 ? info.Title : title;
            session.Updated = info.UpdatedAt;
            _titleBySession[session.Id] = session.Title;
            TileFor(session.Id)?.SetTitle(session.Title);
            SortSessions();
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "rename failed for {Session}", session.Id);
            AddNote($"Rename failed: {ex.Message}", error: true);
        }
    }

    [RelayCommand]
    private async Task DeleteSessionAsync(SessionItem? session)
    {
        if (session is null)
            return;
        if (IsBusy(session.Id))
        {
            AddNote("Stop the running turn before deleting this chat.", error: true);
            return;
        }
        if (ApiFor(session.Id) is not { } api)
            return;
        try
        {
            await api.DeleteSessionAsync(session.Id, CancellationToken.None);
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "delete failed for {Session}", session.Id);
            AddNote($"Delete failed: {ex.Message}", error: true);
            return;
        }

        ForgetSession(session.Id);
        SaveState();
    }

    /// <summary>Unlinks the chat from its workspace; the session itself survives on the server.</summary>
    public void RemoveFromWorkspace(SessionItem session)
    {
        if (IsBusy(session.Id))
        {
            AddNote("Stop the running turn before removing this chat.", error: true);
            return;
        }
        ForgetSession(session.Id);
        SaveState();
    }

    private void ForgetSession(string id)
    {
        DetachTile(id);
        _turnStartBySession.Remove(id);
        _userStoppedBySession.Remove(id);
        _workspaceBySession.Remove(id);
        foreach (var ws in _state.Workspaces)
            ws.Sessions.Remove(id);
        _titleBySession.Remove(id);
        var item = Sessions.FirstOrDefault(s => s.Id == id);
        if (item is not null)
            Sessions.Remove(item);
        _state.Pins.Remove(id);
        DropQueue([id]);
        DropPending([id]);
    }

    private async Task RefreshSessionsAsync()
    {
        if (SelectedWorkspace is not { } ws || !_runtimes.TryGetValue(ws.FolderPath, out var rt))
            return;
        try
        {
            var sessions = await rt.Api.ListSessionsAsync(CancellationToken.None);
            await Dispatcher.UIThread.InvokeAsync(() =>
            {
                if (!ReferenceEquals(SelectedWorkspace, ws))
                    return;
                // only chats this app created under this workspace are listed
                var known = ws.State.Sessions.ToHashSet();
                var mine = sessions
                    .Where(s => known.Contains(s.Id))
                    .OrderByDescending(s => _state.Pins.ContainsKey(s.Id))
                    .ThenByDescending(s => s.UpdatedAt)
                    .ToList();
                var alive = mine.Select(s => s.Id).ToHashSet();
                if (ws.State.Sessions.RemoveAll(id => !alive.Contains(id)) > 0)
                {
                    var gone = known.Where(id => !alive.Contains(id)).ToList();
                    foreach (var id in gone)
                    {
                        _workspaceBySession.Remove(id);
                        _titleBySession.Remove(id);
                        DetachTile(id);
                    }
                    DropQueue(gone);
                    DropPending(gone);
                }
                // an idle fires at the end of every turn; when nothing visible changed,
                // skip the full rebuild + disk save (the common case while chatting)
                var unchanged = Sessions.Count == mine.Count;
                for (var i = 0; unchanged && i < mine.Count; i++)
                {
                    var cur = Sessions[i];
                    var s = mine[i];
                    unchanged = cur.Id == s.Id && cur.Title == s.Title && cur.Updated == s.UpdatedAt &&
                                cur.IsPinned == _state.Pins.ContainsKey(s.Id) &&
                                cur.IsRunning == _runningBySession.Contains(s.Id);
                }
                if (unchanged)
                {
                    SnapSelection();
                    return;
                }
                foreach (var s in mine)
                {
                    _titleBySession[s.Id] = s.Title;
                    TileFor(s.Id)?.SetTitle(s.Title);
                }
                Sessions.Clear();
                foreach (var s in mine)
                    Sessions.Add(new SessionItem(s.Id, s.Title, s.UpdatedAt)
                    {
                        IsPinned = _state.Pins.ContainsKey(s.Id),
                        IsRunning = _runningBySession.Contains(s.Id),
                    });
                foreach (var running in RunningAgents)
                    if (alive.Contains(running.SessionId))
                        running.Title = _titleBySession[running.SessionId];
                SnapSelection();
                SaveState();
            });
        }
        catch (Exception ex)
        {
            // the sidebar is optional; chat still works without it
            Log.Debug(ex, "session list refresh failed");
        }
    }

    private async Task PrimeWorkspaceTitlesAsync(WorkspaceItem ws, WorkspaceRuntime rt)
    {
        try
        {
            var sessions = await rt.Api.ListSessionsAsync(CancellationToken.None);
            var known = ws.State.Sessions.ToHashSet();
            await Dispatcher.UIThread.InvokeAsync(() =>
            {
                foreach (var s in sessions.Where(s => known.Contains(s.Id)))
                    _titleBySession[s.Id] = s.Title;
                foreach (var running in RunningAgents)
                    if (known.Contains(running.SessionId) && _titleBySession.TryGetValue(running.SessionId, out var title))
                        running.Title = title;
            });
        }
        catch (Exception ex)
        {
            Log.Debug(ex, "title prime failed for {Workspace}", ws.Name);
        }
    }

    private void SortSessions()
    {
        var ordered = Sessions
            .OrderByDescending(s => s.IsPinned)
            .ThenByDescending(s => s.Updated)
            .ToList();
        for (var i = 0; i < ordered.Count; i++)
        {
            var index = Sessions.IndexOf(ordered[i]);
            if (index != i)
                Sessions.Move(index, i);
        }
    }

    private void SnapSelection() => UpdateSidebarSelection();

    /// <summary>The selected workspace's sidebar section mirrors the working session list.</summary>
    private void SyncWorkspaceChats()
    {
        if (SelectedWorkspace is not { } ws)
            return;
        ws.Chats.Clear();
        foreach (var chat in Sessions)
            ws.Chats.Add(chat);
        UpdateSidebarSelection();
    }

    /// <summary>Sidebar highlight mirrors the selected workspace and the focused tile's chat,
    /// which may live in another workspace's section.</summary>
    private void UpdateSidebarSelection()
    {
        foreach (var ws in Workspaces)
        {
            ws.IsSelected = ReferenceEquals(ws, SelectedWorkspace);
            foreach (var chat in ws.Chats)
                chat.IsSelected = chat.Id == FocusedTile?.SessionId;
        }
    }

    /// <summary>Loads the sidebar chat list of a workspace started from its context menu.</summary>
    private async Task LoadWorkspaceChatsAsync(WorkspaceItem ws, WorkspaceRuntime rt)
    {
        try
        {
            var sessions = await rt.Api.ListSessionsAsync(CancellationToken.None);
            await Dispatcher.UIThread.InvokeAsync(() =>
            {
                // only chats this app created under this workspace are listed
                var known = ws.State.Sessions.ToHashSet();
                ws.Chats.Clear();
                foreach (var s in sessions.Where(s => known.Contains(s.Id))
                             .OrderByDescending(s => _state.Pins.ContainsKey(s.Id))
                             .ThenByDescending(s => s.UpdatedAt))
                    ws.Chats.Add(new SessionItem(s.Id, s.Title, s.UpdatedAt)
                    {
                        IsPinned = _state.Pins.ContainsKey(s.Id),
                        IsRunning = _runningBySession.Contains(s.Id),
                    });
            });
        }
        catch (Exception ex)
        {
            // the sidebar is optional; chat still works without it
            Log.Debug(ex, "workspace chat list refresh failed for {Workspace}", ws.Name);
        }
    }

    private static string NormalizeDir(string path)
    {
        try
        {
            return Path.GetFullPath(path.Trim()).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        }
        catch (Exception ex)
        {
            Log.Debug(ex, "could not normalize path {Path}", path);
            return path.Trim();
        }
    }

    private async Task LoadModelsAsync(WorkspaceRuntime rt, int gen)
    {
        try
        {
            var models = await rt.Api.GetModelsAsync(CancellationToken.None);
            await Dispatcher.UIThread.InvokeAsync(() =>
            {
                if (gen != _workspaceGeneration)
                    return;
                // clearing ItemsSource echo-writes each tile's Selected*=null; capture and restore per tile
                var restore = OpenTiles.Select(t => (Tile: t, Model: t.SelectedModel, Variant: t.SelectedVariant)).ToList();
                // merge: every running server contributes; existing entries stay for its own tiles
                foreach (var m in models)
                {
                    _variantsByModel[m.Key] = m.Variants.ToList();
                    _contextLimitByModel[m.Key] = m.ContextLimit;
                    if (!Models.Contains(m.Key))
                        Models.Add(m.Key);
                }
                foreach (var entry in restore)
                {
                    entry.Tile.SelectedModel = entry.Model is { } saved && Models.Contains(saved) ? saved : DefaultModelEntry;
                    if (entry.Variant is { } savedVariant && entry.Tile.Variants.Contains(savedVariant))
                        entry.Tile.SelectedVariant = savedVariant;
                }
            });
        }
        catch (Exception ex)
        {
            // the model list is optional; the server default still works
            Log.Debug(ex, "model list refresh failed");
        }
    }

    private async Task LoadAgentsAsync(WorkspaceRuntime rt, int gen)
    {
        try
        {
            var agents = await rt.Api.GetAgentsAsync(CancellationToken.None);
            await Dispatcher.UIThread.InvokeAsync(() =>
            {
                if (gen != _workspaceGeneration)
                    return;
                var restore = OpenTiles.Select(t => (Tile: t, Agent: t.SelectedAgent)).ToList();
                foreach (var agent in agents.Where(a => !a.Hidden && a.Mode is "primary" or "all"))
                    if (!Agents.Contains(agent.Name))
                        Agents.Add(agent.Name);
                foreach (var entry in restore)
                    entry.Tile.SelectedAgent = entry.Agent is { } saved && Agents.Contains(saved)
                        ? saved
                        : DefaultAgentEntry;
            });
        }
        catch (Exception ex)
        {
            // the agent list is optional; the server default (build) still works
            Log.Debug(ex, "agent list refresh failed");
        }
    }

    // ---- event stream ----

    private async Task PumpEventsAsync(OpenCodeApiClient api, CancellationToken ct, WorkspaceItem ws)
    {
        try
        {
            await foreach (var evt in api.StreamEventsAsync(ct))
                HandleEvent(evt, ws);
        }
        catch (OperationCanceledException)
        {
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "event stream ended for {Workspace}", ws.Name);
            PostNote($"[{ws.Name}] event stream ended: {ex.Message}", error: true);
        }
    }

    private void HandleEvent(JsonElement evt, WorkspaceItem ws)
    {
        var type = evt.TryGetProperty("type", out var t) ? t.GetString() : null;
        var data = evt.TryGetProperty("properties", out var p) ? p : evt;
        switch (type)
        {
            case "server.connected":
                PostNote("Connected to the opencode event stream.");
                break;
            case "session.created":
            case "session.updated":
                // child sessions carry parentID: a subagent run spawned by a chat
                if (data.TryGetProperty("info", out var info))
                {
                    var trackedId = info.TryGetProperty("id", out var tracked) ? tracked.GetString() : null;
                    var parentId = info.TryGetProperty("parentID", out var parent) ? parent.GetString() : null;
                    var agent = info.TryGetProperty("agent", out var agentEl) ? agentEl.GetString() : null;
                    Dispatcher.UIThread.Post(() => TrackSessionInfo(trackedId, parentId, agent));
                }
                break;
            case "message.updated":
                Dispatcher.UIThread.Post(() => TileFromData(data, ws)?.OnMessageUpdated(data));
                break;
            case "message.part.updated":
                Dispatcher.UIThread.Post(() => TileFromData(data, ws)?.OnPartUpdated(data));
                break;
            case "permission.asked":
                Dispatcher.UIThread.Post(() => OnPermissionAsked(data, v2: false));
                break;
            case "permission.v2.asked":
                Dispatcher.UIThread.Post(() => OnPermissionAsked(data, v2: true));
                break;
            case "permission.replied":
            case "permission.v2.replied":
                Dispatcher.UIThread.Post(() => OnPermissionReplied(data));
                break;
            case "question.asked":
                Dispatcher.UIThread.Post(() => OnQuestionAsked(data));
                break;
            case "question.replied":
            case "question.rejected":
                Dispatcher.UIThread.Post(() => OnQuestionResolved(data));
                break;
            case "session.status":
                var status = ReadStatus(data);
                var statusSession = ReadSessionId(data);
                var busy = status is "busy" or "retry";
                Dispatcher.UIThread.Post(() =>
                {
                    if (statusSession is { } sid && _subagentBySession.ContainsKey(sid))
                        SetSubagentRunning(sid, busy);
                    else
                        SetRunning(statusSession, busy);
                });
                break;
            case "session.idle":
                var idleSession = ReadSessionId(data);
                Dispatcher.UIThread.Post(() =>
                {
                    SetRunning(idleSession, false);
                    if (idleSession is { } sid)
                    {
                        SetSubagentRunning(sid, false);
                        NoteTurnDuration(sid);
                        // refresh the chat list of the workspace the chat belongs to
                        if (_workspaceBySession.TryGetValue(sid, out var idlePath) &&
                            Workspaces.FirstOrDefault(w => w.FolderPath == idlePath) is { } owner)
                        {
                            if (ReferenceEquals(owner, SelectedWorkspace))
                                _ = RefreshSessionsAsync();
                            else if (_runtimes.TryGetValue(idlePath, out var ort))
                                _ = LoadWorkspaceChatsAsync(owner, ort);
                        }
                        TryDrainQueue(sid);
                    }
                });
                break;
            case "todo.updated":
                Dispatcher.UIThread.Post(() => TileFromData(data, ws)?.OnTodosUpdated(data));
                break;
            case "message.part.removed":
                Dispatcher.UIThread.Post(() => TileFromData(data, ws)?.OnPartRemoved(data));
                break;
            case "session.error":
                var message = ReadError(data);
                var errorSession = ReadSessionId(data);
                Dispatcher.UIThread.Post(() =>
                {
                    if (TileFor(errorSession) is { } tile)
                        tile.AddNote(message, error: true);
                    else
                        AddNote(message, error: true);
                });
                break;
        }
    }

    /// <summary>Turn finished: drop a muted note telling how long the prompt took, send to final answer.
    /// A user-aborted turn says "stopped in" instead — it didn't finish.</summary>
    private void NoteTurnDuration(string sessionId)
    {
        var stopped = _userStoppedBySession.Remove(sessionId);
        if (!_turnStartBySession.Remove(sessionId, out var start))
            return;
        if (TileFor(sessionId) is not { } tile)
            return;
        tile.AddNote($"{(stopped ? "stopped" : "done")} in {ChatParsing.FormatDuration(DateTime.Now - start)}");
    }

    private void SetRunning(string? sessionId, bool running)
    {
        if (string.IsNullOrEmpty(sessionId) || !IsAppSession(sessionId))
            return;
        var changed = running ? _runningBySession.Add(sessionId) : _runningBySession.Remove(sessionId);
        if (!changed)
            return;
        var existing = RunningAgents.FirstOrDefault(r => r.SessionId == sessionId);
        if (running)
        {
            if (existing is null)
            {
                RunningAgents.Add(new RunningAgentItem(sessionId, WorkspaceFor(sessionId), TitleFor(sessionId))
                {
                    NeedsAttention = _pendingBySession.ContainsKey(sessionId),
                });
            }
        }
        else if (existing is not null)
        {
            RunningAgents.Remove(existing);
        }
        RefreshBackgroundFlags();
        UpdateSidebarRunning();
        NotifyBusyChanged();
    }

    /// <summary>Mirrors the running-session set onto sidebar chat rows (drives their spinner).</summary>
    private void UpdateSidebarRunning()
    {
        foreach (var ws in Workspaces)
            foreach (var chat in ws.Chats)
                chat.IsRunning = _runningBySession.Contains(chat.Id);
    }

    private void TrackSessionInfo(string? sessionId, string? parentId, string? agent)
    {
        if (sessionId is null)
            return;
        if (parentId is null || parentId == sessionId)
        {
            if (_subagentBySession.Remove(sessionId))
                SetSubagentRunning(sessionId, false);
            return;
        }
        _subagentBySession[sessionId] = new SubagentInfo(parentId, string.IsNullOrWhiteSpace(agent) ? "subagent" : agent);
        // keep the label of an already-listed run fresh
        if (_runningSubagents.Contains(sessionId) &&
            RunningAgents.FirstOrDefault(r => r.SessionId == sessionId) is { } row)
            row.Title = SubagentRowTitle(sessionId);
    }

    private string SubagentRowTitle(string childId) =>
        _subagentBySession.TryGetValue(childId, out var info)
            ? $"{info.Agent} → {TitleFor(info.ParentSessionId)}"
            : TitleFor(childId);

    /// <summary>Subagent runs surface in the running panel attributed to the chat that spawned them.</summary>
    private void SetSubagentRunning(string childId, bool running)
    {
        if (!_subagentBySession.ContainsKey(childId))
            return;
        var changed = running ? _runningSubagents.Add(childId) : _runningSubagents.Remove(childId);
        if (!changed)
            return;
        var existing = RunningAgents.FirstOrDefault(r => r.SessionId == childId);
        if (running)
        {
            if (existing is null)
                RunningAgents.Add(new RunningAgentItem(childId, WorkspaceFor(childId), SubagentRowTitle(childId))
                {
                    IsSubagent = true,
                    ParentSessionId = _subagentBySession[childId].ParentSessionId,
                });
        }
        else if (existing is not null)
        {
            RunningAgents.Remove(existing);
        }
        RefreshBackgroundFlags();
    }

    /// <summary>Background = running with no open tile: a closed chat that kept going, or another workspace's chat.</summary>
    private void RefreshBackgroundFlags()
    {
        var tiled = OpenTiles.Select(t => t.SessionId).ToHashSet();
        foreach (var item in RunningAgents)
            item.IsBackground = !tiled.Contains(item.SessionId);
        OnPropertyChanged(nameof(HasBackgroundAgents));
        OnPropertyChanged(nameof(AgentsSummary));
        OnSidePanelChanged();
    }

    private void OnPermissionAsked(JsonElement data, bool v2)
    {
        var id = data.TryGetProperty("id", out var i) ? i.GetString() : null;
        var sessionId = data.TryGetProperty("sessionID", out var s) ? s.GetString() : null;
        if (id is null || sessionId is null)
            return;

        string kind, detail;
        string? diff = null;
        if (v2)
        {
            kind = data.TryGetProperty("action", out var a) ? a.GetString() ?? "permission" : "permission";
            detail = data.TryGetProperty("resources", out var r) && r.ValueKind == JsonValueKind.Array
                ? string.Join("\n", r.EnumerateArray().Select(x => x.GetString() ?? "").Where(x => x.Length > 0))
                : "";
        }
        else
        {
            kind = data.TryGetProperty("permission", out var k) ? k.GetString() ?? "permission" : "permission";
            detail = "";
            if (data.TryGetProperty("metadata", out var md) && md.ValueKind == JsonValueKind.Object)
            {
                if (md.TryGetProperty("command", out var c) && c.GetString() is { Length: > 0 } command)
                    detail = command;
                if (md.TryGetProperty("diff", out var d) && d.GetString() is { Length: > 0 } patch)
                    diff = patch;
            }
            if (detail.Length == 0 && data.TryGetProperty("patterns", out var pats) && pats.ValueKind == JsonValueKind.Array)
                detail = string.Join("\n", pats.EnumerateArray().Select(x => x.GetString() ?? "").Where(x => x.Length > 0));
        }
        if (detail.Length == 0)
            detail = "(no detail provided)";

        var item = new PermissionItem
        {
            Id = id,
            SessionId = sessionId,
            Kind = kind,
            Detail = detail,
            Diff = diff,
            IsV2 = v2,
            Respond = RespondToPermission,
        };
        if (TileFor(sessionId) is { } tile)
        {
            tile.ChatItems.Add(item);
            if (AutoAllow)
                _ = AutoAllowCountdown(item);
            return;
        }
        // a subagent's ask belongs to the chat that spawned it
        var owner = _subagentBySession.TryGetValue(sessionId, out var sub) ? sub.ParentSessionId : sessionId;
        // another chat's ask: answer it from its own workspace's server, or park it until its tile opens
        if (!IsAppSession(owner))
            return;
        if (!_pendingBySession.TryGetValue(owner, out var pending))
            _pendingBySession[owner] = pending = [];
        pending.Add(item);
        if (AutoAllow)
            _ = AutoAllowCountdown(item);
        else
            MarkAttention(sessionId, true);
    }

    /// <summary>Random auto-allow wait: quick during working hours, long over lunch and off hours.</summary>
    private (int Min, int Max) AutoAllowRange()
    {
        var hour = DateTime.Now.Hour;
        var away = (hour >= _settings.LunchStartHour && hour < _settings.LunchEndHour)
                || hour >= _settings.OffHoursStartHour || hour < _settings.WorkdayStartHour;
        var (min, max) = away
            ? (_settings.AutoAllowAwayMinSeconds, _settings.AutoAllowAwayMaxSeconds)
            : (_settings.AutoAllowDefaultMinSeconds, _settings.AutoAllowDefaultMaxSeconds);
        min = Math.Max(0, min);
        return (min, Math.Max(min, max));
    }

    /// <summary>Counts down visibly, then answers "always" unless a manual reply beat it
    /// or auto-allow was turned off in the meantime.</summary>
    private async Task AutoAllowCountdown(PermissionItem item)
    {
        var (min, max) = AutoAllowRange();
        var seconds = Random.Shared.Next(min, max + 1);
        for (var left = seconds; left > 0 && item.IsPending; left--)
        {
            item.AutoCountdown = left;
            await Task.Delay(1000);
        }
        item.AutoCountdown = null;
        if (item.IsPending && AutoAllow)
            await RespondToPermission(item, "always", isAuto: true);
    }

    private void FlushPendingPermissions(string sessionId)
    {
        if (!_pendingBySession.Remove(sessionId, out var pending))
            return;
        if (TileFor(sessionId) is { } tile)
            foreach (var item in pending)
                tile.ChatItems.Add(item);
        MarkAttention(sessionId, false);
    }

    private void MarkAttention(string sessionId, bool attention)
    {
        // light the row itself, or the run that spawned it for subagent asks
        var item = RunningAgents.FirstOrDefault(r => r.SessionId == sessionId)
                ?? RunningAgents.FirstOrDefault(r => r.ParentSessionId == sessionId);
        if (item is not null)
            item.NeedsAttention = attention;
        OnPropertyChanged(nameof(HasAttentionNeeded));
    }

    private void OnPermissionReplied(JsonElement data)
    {
        var requestId = data.TryGetProperty("requestID", out var r) ? r.GetString() : null;
        if (requestId is null)
            return;
        foreach (var tile in OpenTiles)
            foreach (var item in tile.ChatItems.OfType<PermissionItem>())
                if (item.Id == requestId && item.IsPending)
                    item.SetAnswer("answered elsewhere", auto: false);
        foreach (var list in _pendingBySession.Values)
            foreach (var item in list)
                if (item.Id == requestId && item.IsPending)
                    item.SetAnswer("answered elsewhere", auto: false);
    }

    private async Task RespondToPermission(PermissionItem item, string response, bool isAuto)
    {
        if (ApiFor(item.SessionId) is not { } api)
        {
            item.SetAnswer("server unavailable", isAuto);
            return;
        }
        // close the buttons immediately so a slow reply cannot be clicked twice
        item.SetAnswer(isAuto ? "auto-allowing…" : "sending…", isAuto);
        try
        {
            if (item.IsV2)
                await api.ReplyV2Async(item.Id, response, CancellationToken.None);
            else
                await api.RespondPermissionAsync(item.SessionId, item.Id, response, CancellationToken.None);
            var label = response switch
            {
                "once" => "allowed (once)",
                "always" => isAuto ? "auto-allowed (always)" : "always allowed",
                _ => "denied",
            };
            item.SetAnswer(label, isAuto);
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "permission reply failed for {Request}", item.Id);
            item.SetAnswer($"reply failed: {ex.Message}", isAuto);
        }
    }

    // ---- question tool asks ----
    // The agent's `question` tool parks the turn until the user answers; never auto-answered.

    private void OnQuestionAsked(JsonElement data)
    {
        if (OpenCodeApiClient.ReadQuestionRequest(data) is not { } request || request.Questions.Count == 0)
            return;
        if (TileFor(request.SessionId) is { } tile)
        {
            if (!tile.ChatItems.OfType<QuestionItem>().Any(i => i.Id == request.Id))
                tile.ChatItems.Add(BuildQuestionItem(request));
            return;
        }
        // a subagent's ask belongs to the chat that spawned it
        var owner = _subagentBySession.TryGetValue(request.SessionId, out var sub) ? sub.ParentSessionId : request.SessionId;
        if (!IsAppSession(owner))
            return;
        if (!_pendingQuestionsBySession.TryGetValue(owner, out var pending))
            _pendingQuestionsBySession[owner] = pending = [];
        if (!pending.Any(i => i.Id == request.Id))
            pending.Add(BuildQuestionItem(request));
        MarkAttention(request.SessionId, true);
    }

    private void OnQuestionResolved(JsonElement data)
    {
        var requestId = data.TryGetProperty("requestID", out var r) ? r.GetString() : null;
        if (requestId is null)
            return;
        var summary = "dismissed";
        if (data.TryGetProperty("answers", out var a) && a.ValueKind == JsonValueKind.Array)
        {
            var answers = a.EnumerateArray()
                .Select(x => string.Join(", ", x.EnumerateArray().Select(l => l.GetString() ?? "").Where(l => l.Length > 0)))
                .Where(x => x.Length > 0)
                .ToList();
            if (answers.Count > 0)
                summary = "answered: " + string.Join(" · ", answers);
        }
        ResolveQuestion(requestId, summary);
    }

    private void ResolveQuestion(string requestId, string summary)
    {
        foreach (var tile in OpenTiles)
            foreach (var item in tile.ChatItems.OfType<QuestionItem>())
                if (item.Id == requestId && item.IsPending)
                    item.SetResolution(summary);
        foreach (var list in _pendingQuestionsBySession.Values)
            foreach (var item in list)
                if (item.Id == requestId && item.IsPending)
                    item.SetResolution(summary);
    }

    private QuestionItem BuildQuestionItem(QuestionRequestInfo request)
    {
        var entries = request.Questions
            .Select(q => new QuestionEntry(q.Question, q.Header, q.Multiple))
            .ToList();
        foreach (var (entry, q) in entries.Zip(request.Questions))
            foreach (var option in q.Options)
                entry.Options.Add(new QuestionOptionItem(option.Label, option.Description));
        return new QuestionItem
        {
            Id = request.Id,
            SessionId = request.SessionId,
            Questions = entries,
            Respond = RespondToQuestion,
            Reject = RejectQuestion,
        };
    }

    private async Task RespondToQuestion(QuestionItem item, string[][] answers)
    {
        if (ApiFor(item.SessionId) is not { } api)
        {
            item.SetResolution("server unavailable");
            return;
        }
        // close the buttons immediately so a slow reply cannot be clicked twice
        var summary = string.Join(" · ", answers.Select(a => a.Length == 0 ? "(empty)" : string.Join(", ", a)));
        item.SetResolution($"sending {summary}…");
        try
        {
            await api.ReplyQuestionAsync(item.Id, answers, CancellationToken.None);
            item.SetResolution(summary);
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "question reply failed for {Request}", item.Id);
            item.SetResolution($"reply failed: {ex.Message}");
        }
    }

    private async Task RejectQuestion(QuestionItem item)
    {
        if (ApiFor(item.SessionId) is not { } api)
        {
            item.SetResolution("server unavailable");
            return;
        }
        item.SetResolution("dismissing…");
        try
        {
            await api.RejectQuestionAsync(item.Id, CancellationToken.None);
            item.SetResolution("dismissed");
        }
        catch (Exception ex)
        {
            Log.Warning(ex, "question dismiss failed for {Request}", item.Id);
            item.SetResolution($"dismiss failed: {ex.Message}");
        }
    }

    private void FlushPendingQuestions(string sessionId)
    {
        if (!_pendingQuestionsBySession.Remove(sessionId, out var pending))
            return;
        if (TileFor(sessionId) is { } tile)
            foreach (var item in pending.Where(i => i.IsPending))
                if (!tile.ChatItems.OfType<QuestionItem>().Any(existing => existing.Id == item.Id))
                    tile.ChatItems.Add(item);
        MarkAttention(sessionId, false);
    }

    /// <summary>Picks up question asks the server still holds for this chat — asked while its tile
    /// was closed or before the app attached. Best effort; live asks arrive on the event stream.</summary>
    private async Task PullPendingQuestionsAsync(string sessionId, OpenCodeApiClient api)
    {
        try
        {
            var pending = await api.GetQuestionsAsync(CancellationToken.None);
            Dispatcher.UIThread.Post(() =>
            {
                if (TileFor(sessionId) is not { } tile)
                    return;
                foreach (var request in pending.Where(q => q.SessionId == sessionId))
                    if (!tile.ChatItems.OfType<QuestionItem>().Any(existing => existing.Id == request.Id))
                        tile.ChatItems.Add(BuildQuestionItem(request));
            });
        }
        catch (Exception ex)
        {
            // the ask stays visible on the server side and re-arrives with its events
            Log.Debug(ex, "pending questions pull failed for {Session}", sessionId);
        }
    }

    private static string? ReadSessionId(JsonElement data) =>
        data.TryGetProperty("sessionID", out var sid) ? sid.GetString() : null;

    private static string ReadStatus(JsonElement data)
    {
        if (!data.TryGetProperty("status", out var status))
            return "";
        if (status.ValueKind == JsonValueKind.String)
            return status.GetString() ?? "";
        return status.ValueKind == JsonValueKind.Object && status.TryGetProperty("type", out var type)
            ? type.GetString() ?? ""
            : "";
    }

    private static string ReadError(JsonElement data)
    {
        if (data.TryGetProperty("error", out var error) && error.ValueKind == JsonValueKind.Object)
        {
            var name = error.TryGetProperty("name", out var n) ? n.GetString() : null;
            if (name is { } n2 && n2.Contains("Abort", StringComparison.OrdinalIgnoreCase))
                return "Turn stopped.";
            // API errors carry the human message nested in data ("...does not include access to…");
            // everything else on the payload (headers, stack, status codes) is transport noise
            var message = "";
            if (error.TryGetProperty("data", out var payload) && payload.ValueKind == JsonValueKind.Object &&
                payload.TryGetProperty("message", out var dm) && dm.GetString() is { Length: > 0 } inner)
                message = inner;
            else if (error.TryGetProperty("message", out var m) && m.GetString() is { Length: > 0 } outer)
                message = outer;
            if (message.Length == 0)
                message = name is { Length: > 0 } n3 ? n3 : "Session error.";
            var text = message.StartsWith("Error:", StringComparison.Ordinal) ? message : $"Error: {message}";
            return text.Length > 600 ? text[..600] + "…" : text;
        }
        return "Session error.";
    }

    private void AddNote(string text, bool error = false) =>
        (FocusedTile ?? OpenTiles.FirstOrDefault())?.AddNote(text, error);

    private void PostNote(string text, bool error = false) => Dispatcher.UIThread.Post(() => AddNote(text, error));

    /// <summary>Errors for a chat without a tile land in the focused transcript tagged with that chat's title.</summary>
    private void PostNote(string sessionId, string text, bool error = false) =>
        Dispatcher.UIThread.Post(() =>
        {
            if (TileFor(sessionId) is { } tile)
                tile.AddNote(text, error);
            else
                AddNote($"{TitleFor(sessionId)}: {text}", error);
        });

    /// <summary>Surface-level messages from the view (clipboard, drag-drop) land in the focused transcript.</summary>
    public void Announce(string text, bool error = false) => AddNote(text, error);
}

/// <summary>A chat whose turn is currently running, any workspace; the side panel lists only background ones.
/// Subagent runs are listed too, attributed to the chat that spawned them.</summary>
public sealed class RunningAgentItem(string sessionId, string workspace, string title) : ObservableObject
{
    private string _title = title;
    private bool _needsAttention;
    private bool _isBackground;
    private bool _isSubagent;

    public string SessionId { get; } = sessionId;

    /// <summary>The chat that spawned this subagent run; null for a chat's own turn.</summary>
    public string? ParentSessionId { get; init; }

    public string Workspace { get; } = workspace;
    public string Title { get => _title; set => SetProperty(ref _title, value); }

    /// <summary>True while no open tile shows this chat, so the side panel is its only surface.</summary>
    public bool IsBackground { get => _isBackground; set => SetProperty(ref _isBackground, value); }

    /// <summary>A subagent (child session) run rather than a chat's own turn.</summary>
    public bool IsSubagent { get => _isSubagent; set => SetProperty(ref _isSubagent, value); }

    /// <summary>A permission ask is waiting in this chat (auto-allow off).</summary>
    public bool NeedsAttention { get => _needsAttention; set => SetProperty(ref _needsAttention, value); }
}
