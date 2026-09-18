using System.Collections.ObjectModel;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Text.Json;
using Avalonia.Threading;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
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
    private readonly Dictionary<string, ChatTileViewModel> _tileBySession = new();
    private readonly List<QueueEntry> _queue = [];
    private int _workspaceGeneration;
    // OpenSessions captured when a workspace is selected; the draft tile a stopped
    // workspace spawns would otherwise persist an empty grid over the saved list
    private List<string> _restoreTiles = [];
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
    public bool CanStartSelected => SelectedWorkspace is not null && !IsServerRunning;
    public bool CanStopSelected => IsServerRunning;
    public bool MultipleWorkspaces => Workspaces.Count > 1;
    public bool HasAgents => Agents.Count > 1;
    public bool HasBackgroundAgents => RunningAgents.Any(r => r.IsBackground);
    public bool HasSidePanel => HasBackgroundAgents || (FocusedTile?.HasTasks ?? false);

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
    }

    internal string SeedModel => _state.Model ?? _settings.DefaultModel ?? DefaultModelEntry;
    internal string SeedVariant => _settings.DefaultVariant ?? DefaultVariantEntry;
    internal string SeedAgent => _state.Agent ?? _settings.DefaultAgent ?? DefaultAgentEntry;

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

    /// <summary>The tile that header actions (Changes, ctx pill) act on.</summary>
    public ChatTileViewModel? FocusedTile
    {
        get => _focusedTile;
        private set
        {
            if (SetProperty(ref _focusedTile, value))
                OnPropertyChanged(nameof(HasSidePanel));
            UpdateSidebarSelection();
        }
    }

    [ObservableProperty] private WorkspaceItem? _selectedWorkspace;
    [ObservableProperty] private string _statusText = "Server stopped";
    [ObservableProperty] private bool _isServerRunning;
    [ObservableProperty] private bool _autoAllow;
    [ObservableProperty] private bool _isSettingsOpen;

    public SettingsPageViewModel SettingsPage { get; }

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
    }

    partial void OnIsServerRunningChanged(bool value)
    {
        OnPropertyChanged(nameof(CanStartSelected));
        OnPropertyChanged(nameof(CanStopSelected));
        OnPropertyChanged(nameof(HasIdleWorkspace));
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
        _switchTask = SwitchWorkspaceAsync(value);
    }

    /// <summary>Opens the clicked sidebar chat: focuses its tile when one exists, otherwise
    /// switches to the chat's workspace and restores it there.</summary>
    public async Task SelectSessionAsync(SessionItem session)
    {
        if (session.Id == FocusedTile?.SessionId)
            return;
        if (TileFor(session.Id) is { } existing)
        {
            FocusTile(existing);
            return;
        }
        if (_workspaceBySession.TryGetValue(session.Id, out var path) &&
            Workspaces.FirstOrDefault(w => w.FolderPath == path) is { } owner &&
            !ReferenceEquals(owner, SelectedWorkspace))
        {
            SelectedWorkspace = owner; // the change hook stores its switch task in _switchTask
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
        var current = SelectedWorkspace?.Chats.FirstOrDefault(s => s.Id == session.Id) ?? session;
        await OpenSessionAsync(current);
    }

    public void SelectWorkspace(WorkspaceItem ws)
    {
        if (!ReferenceEquals(SelectedWorkspace, ws))
            SelectedWorkspace = ws; // the change hook stores its switch task in _switchTask
    }

    private void OnSidePanelChanged()
    {
        OnPropertyChanged(nameof(HasBackgroundAgents));
        OnPropertyChanged(nameof(HasSidePanel));
    }

    public MainViewModel()
    {
        _state = UiState.Load();
        _settings = AppSettings.Load();
        // one-time migration: the pre-settings auto-allow choice moves into settings.json
        _settings.AutoAllowAlways ??= _state.AutoAllowAlways;
        SettingsPage = new SettingsPageViewModel(this);
        SettingsPage.Saved += (_, _) => IsSettingsOpen = false;
        if (_state.Workspaces.Count == 0 && !string.IsNullOrWhiteSpace(_state.Folder))
            _state.Workspaces.Add(new WorkspaceState { Path = NormalizeDir(_state.Folder) });
        foreach (var ws in _state.Workspaces)
        {
            var item = new WorkspaceItem(ws) { StateChanged = SaveState };
            Workspaces.Add(item);
            foreach (var sid in ws.Sessions)
                _workspaceBySession[sid] = ws.Path;
        }
        _autoAllow = _state.AutoAllowAlways ?? true;
        Sessions.CollectionChanged += (_, _) => SyncWorkspaceChats();
        Workspaces.CollectionChanged += (_, _) =>
        {
            OnPropertyChanged(nameof(HasNoWorkspace));
            OnPropertyChanged(nameof(HasIdleWorkspace));
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
        };
        // restore the last workspace selection but never auto-start a server
        var selected = Workspaces.FirstOrDefault(w => w.FolderPath == _state.ActiveWorkspace) ?? Workspaces.FirstOrDefault();
        if (selected is not null)
            SelectedWorkspace = selected;
    }

    private void SaveState()
    {
        _state.ActiveWorkspace = SelectedWorkspace?.FolderPath;
        _state.AutoAllowAlways = AutoAllow;
        PersistTiles();
        _state.Save();
    }

    /// <summary>Writes the selected workspace's tiled chats and focused session; only while its
    /// data is actually loaded — a stopped workspace keeps its saved list untouched.</summary>
    private void PersistTiles()
    {
        if (SelectedWorkspace is not { } ws || !_runtimes.ContainsKey(ws.FolderPath))
            return;
        ws.State.OpenSessions = OpenTiles
            .Where(t => t.SessionId is not null)
            .Select(t => t.SessionId!)
            .ToList();
        ws.State.ActiveSession = FocusedTile?.SessionId;
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
        _workspaceBySession.TryGetValue(sessionId, out var path) && _runtimes.TryGetValue(path, out var rt) ? rt.Api : null;

    private bool IsAppSession(string sessionId) => _workspaceBySession.ContainsKey(sessionId);

    private string TitleFor(string sessionId) =>
        _titleBySession.TryGetValue(sessionId, out var title)
            ? title
            : $"agent {sessionId[..Math.Min(8, sessionId.Length)]}";

    internal ChatTileViewModel? TileFor(string? sessionId) =>
        sessionId is { } id && _tileBySession.TryGetValue(id, out var tile) ? tile : null;

    private ChatTileViewModel? TileFromData(JsonElement data)
    {
        if (ReadSessionId(data) is { } id)
            return TileFor(id);
        // events without a session id belong to whatever is open
        return FocusedTile;
    }

    // ---- tile grid: open, close, focus ----

    public void FocusTile(ChatTileViewModel? tile)
    {
        if (ReferenceEquals(FocusedTile, tile))
            return;
        if (FocusedTile is { } old)
            old.IsFocused = false;
        FocusedTile = tile;
        if (SelectedWorkspace is { } ws)
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
        OpenTiles.Remove(tile);
        if (tile.SessionId is { } id)
            _tileBySession.Remove(id);
        if (!ReferenceEquals(FocusedTile, tile))
            return;
        var next = OpenTiles.LastOrDefault();
        if (next is null && SelectedWorkspace is not null)
        {
            next = new ChatTileViewModel(this);
            OpenTiles.Add(next);
        }
        FocusTile(next);
    }

    private void DetachTile(string sessionId)
    {
        if (TileFor(sessionId) is { } tile)
            DetachTile(tile);
    }

    private void ClearTiles()
    {
        foreach (var tile in OpenTiles)
            tile.IsFocused = false;
        OpenTiles.Clear();
        _tileBySession.Clear();
        FocusedTile = null;
    }

    private void AddDraftTile()
    {
        if (SelectedWorkspace is null)
            return;
        var tile = new ChatTileViewModel(this);
        OpenTiles.Add(tile);
        FocusTile(tile);
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
            tile.ChatItems.Add(entry.Item);
    }

    [RelayCommand]
    private async Task NewChatAsync()
    {
        if (SelectedWorkspace is null)
            return;
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
        if (!_runtimes.ContainsKey(SelectedWorkspace.FolderPath))
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
        var wasSelected = ReferenceEquals(SelectedWorkspace, ws);
        if (_runtimes.TryGetValue(ws.FolderPath, out var rt))
        {
            rt.Events?.Cancel();
            rt.Server.Stop();
            _runtimes.Remove(ws.FolderPath);
        }
        foreach (var sid in ws.State.Sessions)
            SetRunning(sid, false);
        DropQueue(ws.State.Sessions);
        DropPending(ws.State.Sessions);
        foreach (var sid in ws.State.Sessions)
        {
            _workspaceBySession.Remove(sid);
            _titleBySession.Remove(sid);
        }
        _state.Workspaces.Remove(ws.State);
        Workspaces.Remove(ws);
        if (wasSelected)
        {
            ClearTiles();
            Sessions.Clear();
            IsServerRunning = false;
            StatusText = "Server stopped";
        }
        SaveState();
    }

    [RelayCommand]
    private async Task StartWorkspaceAsync(WorkspaceItem? ws)
    {
        if (ws is null || (_runtimes.TryGetValue(ws.FolderPath, out var existing) && existing.Server.IsRunning))
            return;
        StatusText = $"Starting opencode serve — {ws.Name}…";
        try
        {
            var port = OpenCodeServer.FindFreePort();
            var server = new OpenCodeServer();
            await server.StartAsync(_settings.OpenCodeExecutable, ws.FolderPath, port, s => Debug.WriteLine($"[opencode] {s}"), CancellationToken.None);
            var rt = new WorkspaceRuntime { Server = server, Api = new OpenCodeApiClient(server.BaseUrl) };
            _runtimes[ws.FolderPath] = rt;
            ws.IsRunning = true;
            rt.Events = new CancellationTokenSource();
            _ = PumpEventsAsync(rt.Api, rt.Events.Token, ws.Name);
            if (!ReferenceEquals(ws, SelectedWorkspace))
            {
                // started from the context menu; the open workspace's tiles stay untouched
                StatusText = $"Server ready — {ws.Name} ({server.BaseUrl})";
                _ = PrimeWorkspaceTitlesAsync(ws, rt);
                _ = LoadWorkspaceChatsAsync(ws, rt);
                return;
            }
            IsServerRunning = true;
            StatusText = (server.Spawned ? "Server ready — " : "Attached — ") + server.BaseUrl;
            var wanted = _restoreTiles.Count > 0 ? _restoreTiles : ws.State.OpenSessions.ToList();
            _restoreTiles = [];
            await LoadWorkspaceDataAsync(ws, rt, wanted);
        }
        catch (Exception ex)
        {
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
        ws.IsRunning = false;
        ws.Chats.Clear();
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

    private async Task SwitchWorkspaceAsync(WorkspaceItem ws)
    {
        // captured before anything can persist an empty grid over the saved list
        _restoreTiles = ws.State.OpenSessions.ToList();
        var wanted = _restoreTiles;
        ClearTiles();
        Sessions.Clear();
        if (!_runtimes.TryGetValue(ws.FolderPath, out var rt) || !rt.Server.IsRunning)
        {
            IsServerRunning = false;
            StatusText = $"Server stopped — {ws.Name}";
            AddDraftTile();
            return;
        }
        IsServerRunning = true;
        StatusText = (rt.Server.Spawned ? "Server ready — " : "Attached — ") + rt.Server.BaseUrl;
        await LoadWorkspaceDataAsync(ws, rt, wanted);
    }

    /// <summary>Models, agents and the tiled chats of a workspace whose server is already up.</summary>
    private async Task LoadWorkspaceDataAsync(WorkspaceItem ws, WorkspaceRuntime rt, List<string> wanted)
    {
        var gen = ++_workspaceGeneration;
        _ = LoadModelsAsync(rt, gen);
        _ = LoadAgentsAsync(rt, gen);
        await RefreshSessionsAsync();
        await RestoreOpenTilesAsync(ws, rt, gen, wanted);
    }

    private async Task RestoreOpenTilesAsync(WorkspaceItem ws, WorkspaceRuntime rt, int gen, List<string> wanted)
    {
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
                ShowQueuedFor(tile);
            }
            catch (Exception ex)
            {
                if (gen != _workspaceGeneration || !ReferenceEquals(SelectedWorkspace, ws))
                    return;
                AddNote($"Could not load chat: {ex.Message}", error: true);
            }
        }
        if (gen != _workspaceGeneration || !ReferenceEquals(SelectedWorkspace, ws))
            return;
        PruneUntouchedDrafts();
        if (OpenTiles.Count == 0)
        {
            AddDraftTile();
            return;
        }
        var focus = ws.State.ActiveSession is { } active && TileFor(active) is { } activeTile
            ? activeTile
            : OpenTiles.Last();
        FocusTile(focus);
    }

    // ---- chats ----

    [RelayCommand]
    private async Task OpenSessionAsync(SessionItem? session)
    {
        if (session is null)
            return;
        if (TileFor(session.Id) is { } existing)
        {
            FocusTile(existing);
            return;
        }
        if (SelectedWorkspace is not { } ws || !_runtimes.TryGetValue(ws.FolderPath, out var rt))
        {
            SnapSelection();
            return;
        }
        try
        {
            var messages = await rt.Api.GetMessagesAsync(session.Id, CancellationToken.None);
            if (!ReferenceEquals(SelectedWorkspace, ws) || !_runtimes.TryGetValue(ws.FolderPath, out rt))
                return;
            var tile = new ChatTileViewModel(this);
            tile.SetSession(session.Id, TitleFor(session.Id));
            _tileBySession[session.Id] = tile;
            OpenTiles.Add(tile);
            tile.Restore(messages);
            FlushPendingPermissions(session.Id);
            ShowQueuedFor(tile);
            ws.State.ActiveSession = session.Id;
            FocusTile(tile);
        }
        catch (Exception ex)
        {
            AddNote($"Could not load chat: {ex.Message}", error: true);
            SnapSelection();
        }
    }

    /// <summary>Opens a chat from the running-agents panel, switching workspaces if needed.</summary>
    public async Task OpenRunningAsync(RunningAgentItem running)
    {
        if (!_workspaceBySession.TryGetValue(running.SessionId, out var path))
            return;
        var ws = Workspaces.FirstOrDefault(w => w.FolderPath == path);
        if (ws is null)
            return;
        if (!ReferenceEquals(SelectedWorkspace, ws))
            SelectedWorkspace = ws; // the change hook stores its switch task in _switchTask
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
        var session = Sessions.FirstOrDefault(s => s.Id == running.SessionId);
        if (session is not null)
            await OpenSessionAsync(session);
    }

    internal async Task SendFromTileAsync(ChatTileViewModel tile)
    {
        var text = tile.Input.Trim();
        if ((text.Length == 0 && tile.Attachments.Count == 0) || !IsServerRunning || SelectedApi is null)
            return;
        RecordPrompt(text);
        tile.ResetPromptHistory();
        var attachments = tile.TakeAttachments();
        tile.Input = "";
        if (tile.IsBusy)
        {
            // park the message on this chat; it is sent when that chat goes idle
            var item = new QueuedMessageItem(text, attachments.Count) { Remove = RemoveQueued };
            _queue.Add(new QueueEntry(tile.SessionId!, text, attachments, item));
            tile.ChatItems.Add(item);
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
                PostNote(sessionId, $"Message failed: {ex.Message}", error: true);
            }
        });
    }

    private void RemoveQueued(QueuedMessageItem item)
    {
        var index = _queue.FindIndex(e => ReferenceEquals(e.Item, item));
        if (index >= 0)
            _queue.RemoveAt(index);
        foreach (var tile in OpenTiles)
            tile.ChatItems.Remove(item);
    }

    private void DropQueue(IReadOnlyCollection<string> sessionIds)
    {
        for (var i = _queue.Count - 1; i >= 0; i--)
        {
            if (!sessionIds.Contains(_queue[i].SessionId))
                continue;
            var entry = _queue[i];
            TileFor(entry.SessionId)?.ChatItems.Remove(entry.Item);
            _queue.RemoveAt(i);
        }
    }

    private void DropPending(IReadOnlyCollection<string> sessionIds)
    {
        foreach (var sid in sessionIds)
            if (_pendingBySession.Remove(sid, out _))
                MarkAttention(sid, false);
    }

    /// <summary>Sends the oldest queued message after that chat finishes its turn; called on session.idle.</summary>
    private void TryDrainQueue(string sessionId)
    {
        if (ApiFor(sessionId) is null)
            return;
        var entry = _queue.FirstOrDefault(e => e.SessionId == sessionId);
        if (entry is null)
            return;
        _queue.Remove(entry);
        TileFor(sessionId)?.ChatItems.Remove(entry.Item);
        SendCoreAsync(sessionId, entry.Text, entry.Attachments);
    }

    internal async Task AbortTileAsync(ChatTileViewModel tile)
    {
        if (tile.SessionId is not { } id || !IsBusy(id) || ApiFor(id) is not { } api)
            return;
        try
        {
            await api.AbortAsync(id, CancellationToken.None);
            SetRunning(id, false);
        }
        catch (Exception ex)
        {
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
            SetRunning(sessionId, false);
        }
        catch (Exception ex)
        {
            PostNote(sessionId, $"Abort failed: {ex.Message}", error: true);
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
            catch
            {
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
                    });
                foreach (var running in RunningAgents)
                    if (alive.Contains(running.SessionId))
                        running.Title = _titleBySession[running.SessionId];
                SnapSelection();
                SaveState();
            });
        }
        catch
        {
            // the sidebar is optional; chat still works without it
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
        catch
        {
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

    /// <summary>Sidebar highlight mirrors the selected workspace and the focused tile's chat.</summary>
    private void UpdateSidebarSelection()
    {
        foreach (var ws in Workspaces)
        {
            ws.IsSelected = ReferenceEquals(ws, SelectedWorkspace);
            foreach (var chat in ws.Chats)
                chat.IsSelected = ws.IsSelected && chat.Id == FocusedTile?.SessionId;
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
                    });
            });
        }
        catch
        {
            // the sidebar is optional; chat still works without it
        }
    }

    private static string NormalizeDir(string path)
    {
        try
        {
            return Path.GetFullPath(path.Trim()).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        }
        catch
        {
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
                Models.Clear();
                Models.Add(DefaultModelEntry);
                _variantsByModel = models.ToDictionary(m => m.Key, m => m.Variants.ToList());
                _contextLimitByModel = models.ToDictionary(m => m.Key, m => m.ContextLimit, StringComparer.OrdinalIgnoreCase);
                foreach (var m in models)
                    Models.Add(m.Key);
                foreach (var entry in restore)
                {
                    entry.Tile.SelectedModel = entry.Model is { } saved && Models.Contains(saved) ? saved : DefaultModelEntry;
                    if (entry.Variant is { } savedVariant && entry.Tile.Variants.Contains(savedVariant))
                        entry.Tile.SelectedVariant = savedVariant;
                }
            });
        }
        catch
        {
            // the model list is optional; the server default still works
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
                Agents.Clear();
                Agents.Add(DefaultAgentEntry);
                foreach (var agent in agents.Where(a => !a.Hidden && a.Mode is "primary" or "all"))
                    Agents.Add(agent.Name);
                foreach (var entry in restore)
                    entry.Tile.SelectedAgent = entry.Agent is { } saved && Agents.Contains(saved)
                        ? saved
                        : DefaultAgentEntry;
            });
        }
        catch
        {
            // the agent list is optional; the server default (build) still works
        }
    }

    /// <summary>Shares the session and returns the public URL for the caller to copy; null when the server gave none.</summary>
    public async Task<string?> ShareSessionAsync(SessionItem session)
    {
        if (ApiFor(session.Id) is not { } api)
            return null;
        try
        {
            return await api.ShareAsync(session.Id, CancellationToken.None);
        }
        catch (Exception ex)
        {
            AddNote($"Share failed: {ex.Message}", error: true);
            return null;
        }
    }

    public async Task UnshareSessionAsync(SessionItem session)
    {
        if (ApiFor(session.Id) is not { } api)
            return;
        try
        {
            await api.UnshareAsync(session.Id, CancellationToken.None);
            AddNote("Session is no longer shared.");
        }
        catch (Exception ex)
        {
            AddNote($"Unshare failed: {ex.Message}", error: true);
        }
    }

    // ---- event stream ----

    private async Task PumpEventsAsync(OpenCodeApiClient api, CancellationToken ct, string workspaceName)
    {
        try
        {
            await foreach (var evt in api.StreamEventsAsync(ct))
                HandleEvent(evt);
        }
        catch (OperationCanceledException)
        {
        }
        catch (Exception ex)
        {
            PostNote($"[{workspaceName}] event stream ended: {ex.Message}", error: true);
        }
    }

    private void HandleEvent(JsonElement evt)
    {
        var type = evt.TryGetProperty("type", out var t) ? t.GetString() : null;
        var data = evt.TryGetProperty("properties", out var p) ? p : evt;
        switch (type)
        {
            case "server.connected":
                PostNote("Connected to the opencode event stream.");
                break;
            case "message.updated":
                Dispatcher.UIThread.Post(() => TileFromData(data)?.OnMessageUpdated(data));
                break;
            case "message.part.updated":
                Dispatcher.UIThread.Post(() => TileFromData(data)?.OnPartUpdated(data));
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
            case "session.status":
                var status = ReadStatus(data);
                var statusSession = ReadSessionId(data);
                Dispatcher.UIThread.Post(() => SetRunning(statusSession, status is "busy" or "retry"));
                break;
            case "session.idle":
                var idleSession = ReadSessionId(data);
                Dispatcher.UIThread.Post(() =>
                {
                    SetRunning(idleSession, false);
                    if (idleSession is not { } sid)
                        return;
                    if (sid == FocusedTile?.SessionId)
                        _ = RefreshSessionsAsync();
                    TryDrainQueue(sid);
                });
                break;
            case "todo.updated":
                Dispatcher.UIThread.Post(() => TileFromData(data)?.OnTodosUpdated(data));
                break;
            case "message.part.removed":
                Dispatcher.UIThread.Post(() => TileFromData(data)?.OnPartRemoved(data));
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
                var workspace = _workspaceBySession.TryGetValue(sessionId, out var path) &&
                                Workspaces.FirstOrDefault(w => w.FolderPath == path) is { } wsItem
                    ? wsItem.Name
                    : "";
                RunningAgents.Add(new RunningAgentItem(sessionId, workspace, TitleFor(sessionId))
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
        NotifyBusyChanged();
    }

    /// <summary>Background = running with no open tile: a closed chat that kept going, or another workspace's chat.</summary>
    private void RefreshBackgroundFlags()
    {
        var tiled = OpenTiles.Select(t => t.SessionId).ToHashSet();
        foreach (var item in RunningAgents)
            item.IsBackground = !tiled.Contains(item.SessionId);
        OnPropertyChanged(nameof(HasBackgroundAgents));
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
                _ = RespondToPermission(item, "always", isAuto: true);
            return;
        }
        // another chat's ask: answer it from its own workspace's server, or park it until its tile opens
        if (!IsAppSession(sessionId))
            return;
        if (AutoAllow)
        {
            _ = RespondToPermission(item, "always", isAuto: true);
            return;
        }
        if (!_pendingBySession.TryGetValue(sessionId, out var pending))
            _pendingBySession[sessionId] = pending = [];
        pending.Add(item);
        MarkAttention(sessionId, true);
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
        var item = RunningAgents.FirstOrDefault(r => r.SessionId == sessionId);
        if (item is not null)
            item.NeedsAttention = attention;
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
            item.SetAnswer($"reply failed: {ex.Message}", isAuto);
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
            var message = error.TryGetProperty("message", out var m) ? m.GetString() : null;
            if (name is { } n2 && n2.Contains("Abort", StringComparison.OrdinalIgnoreCase))
                return "Turn stopped.";
            var head = $"Error: {name ?? "opencode"}{(message is { Length: > 0 } ? $" — {message}" : "")}";
            var extras = new StringBuilder();
            foreach (var prop in error.EnumerateObject())
            {
                if (prop.Name is "name" or "message")
                    continue;
                var raw = prop.Value.ValueKind == JsonValueKind.String ? prop.Value.GetString() : prop.Value.GetRawText();
                if (string.IsNullOrEmpty(raw))
                    continue;
                extras.Append('\n').Append(prop.Name).Append(": ").Append(raw);
            }
            var detail = extras.ToString();
            if (detail.Length > 2000)
                detail = detail[..2000] + "\n… (truncated)";
            return detail.Length > 0 ? head + detail : head;
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

/// <summary>One touched file in the Changes flyout; Patch is the unified diff when known.</summary>
public sealed record FileChange(string FilePath, string? Patch, int Additions, int Deletions)
{
    public string Name => Path.GetFileName(FilePath);
    public bool HasPatch => Patch is { Length: > 0 };
}

/// <summary>A chat whose turn is currently running, any workspace; the side panel lists only background ones.</summary>
public sealed class RunningAgentItem(string sessionId, string workspace, string title) : ObservableObject
{
    private string _title = title;
    private bool _needsAttention;
    private bool _isBackground;

    public string SessionId { get; } = sessionId;
    public string Workspace { get; } = workspace;
    public string Title { get => _title; set => SetProperty(ref _title, value); }

    /// <summary>True while no open tile shows this chat, so the side panel is its only surface.</summary>
    public bool IsBackground { get => _isBackground; set => SetProperty(ref _isBackground, value); }

    /// <summary>A permission ask is waiting in this chat (auto-allow off).</summary>
    public bool NeedsAttention { get => _needsAttention; set => SetProperty(ref _needsAttention, value); }
}
