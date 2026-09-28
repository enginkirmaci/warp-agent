using System.Collections.ObjectModel;
using System.Text.Json;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using WarpAgent.Models;
using WarpAgent.Services;

namespace WarpAgent.ViewModels;

/// <summary>
/// One chat tile in the grid: transcript, composer and stream state for a single session.
/// MainViewModel routes SSE events here by session id and owns workspaces, runtimes and the sidebar.
/// </summary>
public partial class ChatTileViewModel : ObservableObject
{
    private readonly MainViewModel _owner;
    private readonly Dictionary<string, string> _roleByMessage = new();
    private readonly Dictionary<string, long> _createdByMessage = new();
    private readonly Dictionary<string, string> _metaByMessage = new();
    private readonly Dictionary<string, List<AssistantTextItem>> _textsByMessage = new();
    private readonly Dictionary<string, AssistantTextItem> _textByPart = new();
    private readonly Dictionary<string, ToolItem> _toolByPart = new();
    private readonly Dictionary<string, ReasoningItem> _reasoningByPart = new();
    private readonly Dictionary<string, ImageItem> _imageByPart = new();
    private string? _lastUsedModel;
    // token totals per assistant message in arrival order; the ctx pill shows the
    // LATEST one — each message's tokens already describe that call's full context
    // (cache reads included), so summing turns would multiply-count the window
    private readonly Dictionary<string, long> _tokensByMessage = new();
    private readonly List<string> _tokenOrder = new();
    private int _contextLimit;
    private bool _isFocused;
    private string _title = MainViewModel.DraftTitle;

    public ChatTileViewModel(MainViewModel owner)
    {
        _owner = owner;
        _selectedModel = owner.SeedModel;
        _selectedAgent = owner.SeedAgent;
        RefreshVariants(seedFromSettings: true);
        ChatItems.CollectionChanged += (_, _) => OnPropertyChanged(nameof(HasMessages));
        Attachments.CollectionChanged += (_, _) => OnPropertyChanged(nameof(HasAttachments));
        QueueRows.CollectionChanged += (_, _) => OnPropertyChanged(nameof(HasQueueRows));
        Variants.CollectionChanged += (_, _) => OnPropertyChanged(nameof(HasVariants));
        Tasks.CollectionChanged += (_, _) =>
        {
            OnPropertyChanged(nameof(HasTasks));
            OnPropertyChanged(nameof(TasksProgress));
            OnPropertyChanged(nameof(HasTasksOrAgents));
            OnPropertyChanged(nameof(HasTasksAndAgents));
        };
        TileAgents.CollectionChanged += (_, _) =>
        {
            OnPropertyChanged(nameof(HasTileAgents));
            OnPropertyChanged(nameof(TileAgentsSummary));
            OnPropertyChanged(nameof(HasTasksOrAgents));
            OnPropertyChanged(nameof(HasTasksAndAgents));
        };
        // the flyout lists only this chat's runs; mirror the owner's running set as it changes
        _owner.RunningAgents.CollectionChanged += OnOwnerRunningChanged;
    }

    private void OnOwnerRunningChanged(object? sender, System.Collections.Specialized.NotifyCollectionChangedEventArgs e) =>
        RefreshTileAgents();

    /// <summary>Cuts the subscription to the app-lifetime owner when the tile leaves the grid;
    /// without it every closed tile (with its whole transcript) stays rooted forever.</summary>
    internal void DetachFromOwner() => _owner.RunningAgents.CollectionChanged -= OnOwnerRunningChanged;

    public MainViewModel Owner => _owner;

    public ObservableCollection<object> ChatItems { get; } = [];
    public ObservableCollection<AttachmentItem> Attachments { get; } = [];
    public ObservableCollection<string> Variants { get; } = [];
    public ObservableCollection<TaskItem> Tasks { get; } = [];

    /// <summary>Subagent runs spawned by THIS chat — the flyout's Agents section is scoped here,
    /// never another tile's agents or chats running without a tile.</summary>
    public ObservableCollection<RunningAgentItem> TileAgents { get; } = [];

    public bool HasTileAgents => TileAgents.Count > 0;

    /// <summary>"N running" counter for the flyout's Agents header, scoped to this chat.</summary>
    public string TileAgentsSummary
    {
        get
        {
            var n = TileAgents.Count;
            return n == 1 ? "1 running" : $"{n} running";
        }
    }

    /// <summary>Queued prompts render pinned above the composer, not interleaved in the transcript.</summary>
    public ObservableCollection<QueuedMessageItem> QueueRows { get; } = [];

    public bool HasQueueRows => QueueRows.Count > 0;

    public bool HasMessages => ChatItems.Count > 0;
    public bool HasAttachments => Attachments.Count > 0;
    public bool HasVariants => Variants.Count > 1;
    public bool HasTasks => Tasks.Count > 0;

    /// <summary>"3/5" completed counter for the flyout's Progress header.</summary>
    public string TasksProgress => $"{Tasks.Count(t => t.Done)}/{Tasks.Count}";

    /// <summary>Drives the header tasks button: this chat's tasks or the subagent runs it spawned.</summary>
    public bool HasTasksOrAgents => HasTasks || HasTileAgents;

    /// <summary>Both sections visible in the flyout — shows the divider between them.</summary>
    public bool HasTasksAndAgents => HasTasks && HasTileAgents;

    /// <summary>Null while the tile is an unsent draft; the session is created on first send.</summary>
    public string? SessionId { get; private set; }

    public string Title
    {
        get => _title;
        private set => SetProperty(ref _title, value);
    }

    public bool IsFocused
    {
        get => _isFocused;
        internal set => SetProperty(ref _isFocused, value);
    }

    /// <summary>Busy state of this chat only; other tiles keep streaming independently.</summary>
    public bool IsBusy => SessionId is { } id && _owner.IsBusy(id);

    internal void RefreshBusy() => OnPropertyChanged(nameof(IsBusy));

    internal bool IsUntouched => SessionId is null && Input.Length == 0 && Attachments.Count == 0 && ChatItems.Count == 0;

    internal void SetSession(string id, string title)
    {
        SessionId = id;
        Title = title;
        RefreshConnectivity();
        RefreshTileAgents();
    }

    /// <summary>Mirrors the owner's running set into this chat's flyout: only rows whose parent
    /// is this session (the subagent runs it spawned) — never other tiles' agents.</summary>
    private void RefreshTileAgents()
    {
        TileAgents.Clear();
        foreach (var run in _owner.RunningAgents.Where(r => r.ParentSessionId == SessionId))
            TileAgents.Add(run);
    }

    internal void SetTitle(string title) => Title = title;

    /// <summary>Whether this tile can reach a server: an existing chat uses its own
    /// workspace's server, a draft uses the selected workspace's one.</summary>
    public bool IsConnected => SessionId is { } id ? _owner.HasRuntime(id) : _owner.IsServerRunning;

    internal void RefreshConnectivity() => OnPropertyChanged(nameof(IsConnected));

    public void Focus() => _owner.FocusTile(this);

    [ObservableProperty] private string _input = "";

    // prompt-history browsing: 0 = not browsing, N = N steps back from the newest entry
    private int _historyIndex;
    private string _historyDraft = "";
    private bool _suppressHistoryReset;    [ObservableProperty] private string? _selectedModel;
    [ObservableProperty] private string? _selectedVariant;
    [ObservableProperty] private string? _selectedAgent;
    [ObservableProperty] private string? _contextText;
    [ObservableProperty] private bool _autoTitlePending;

    public bool HasContext => ContextText is not null;

    partial void OnContextTextChanged(string? value) => OnPropertyChanged(nameof(HasContext));

    partial void OnInputChanged(string? value)
    {
        if (!_suppressHistoryReset)
            _historyIndex = 0;
    }

    /// <summary>Terminal-style Up/Down cycling through previously sent prompts; false lets the caret move.</summary>
    internal bool CyclePromptHistory(bool up)
    {
        var count = _owner.PromptHistoryCount;
        if (up)
        {
            if (_historyIndex == 0)
            {
                if (count == 0)
                    return false;
                _historyDraft = Input;
                _historyIndex = 1;
            }
            else if (_historyIndex < count)
                _historyIndex++;
            SetInput(_owner.PeekPromptHistory(_historyIndex - 1));
        }
        else
        {
            if (_historyIndex == 0)
                return false;
            _historyIndex--;
            SetInput(_historyIndex == 0 ? _historyDraft : _owner.PeekPromptHistory(_historyIndex - 1));
        }
        return true;
    }

    private void SetInput(string text)
    {
        _suppressHistoryReset = true;
        Input = text;
        _suppressHistoryReset = false;
    }

    internal void ResetPromptHistory() => _historyIndex = 0;

    partial void OnSelectedModelChanged(string? value)
    {
        OnPropertyChanged(nameof(SelectedModelShort));
        RefreshVariants();
        _owner.PersistComposer(this);
    }

    /// <summary>Display-only name for the composer pill: the segment after the last '/'
    /// ("zai-coding-plan/glm-5.3-flash" → "glm-5.3-flash"); SelectedModel keeps the full id.</summary>
    public string SelectedModelShort
    {
        get
        {
            if (SelectedModel is not { } m)
                return string.Empty;
            var i = m.LastIndexOf('/');
            return i >= 0 ? m[(i + 1)..] : m;
        }
    }

    partial void OnSelectedVariantChanged(string? value) => _owner.PersistComposer(this);

    partial void OnSelectedAgentChanged(string? value) => _owner.PersistComposer(this);

    /// <summary>Shift+Tab in the composer: next entry in the agent list, wrapping;
    /// the "(default build)" sentinel participates in the cycle like any other entry.</summary>
    internal void CycleAgent()
    {
        if (!_owner.HasAgents)
            return;
        var agents = _owner.Agents;
        var index = SelectedAgent is { } current ? agents.IndexOf(current) : -1;
        SelectedAgent = agents[(index + 1) % agents.Count];
    }

    private void RefreshVariants(bool seedFromSettings = false)
    {
        Variants.Clear();
        Variants.Add(MainViewModel.DefaultVariantEntry);
        if (SelectedModel is { } model && _owner.TryGetVariants(model, out var variants))
            foreach (var variant in variants)
                Variants.Add(variant);
        SelectedVariant = seedFromSettings ? _owner.SeedVariant : MainViewModel.DefaultVariantEntry;
    }

    [RelayCommand]
    private Task SendAsync() => _owner.SendFromTileAsync(this);

    [RelayCommand]
    private Task AbortAsync() => _owner.AbortTileAsync(this);

    [RelayCommand]
    private void Close() => _owner.CloseTile(this);

    [RelayCommand]
    private void RemoveAttachment(AttachmentItem? attachment)
    {
        if (attachment is not null)
            Attachments.Remove(attachment);
    }

    public void AttachImage(byte[] png) =>
        Attachments.Add(new AttachmentItem($"paste-{Attachments.Count + 1}.png", "image/png", png));

    public void AttachFile(string fileName, string mime, byte[] data) =>
        Attachments.Add(new AttachmentItem(fileName, mime, data));

    public void AppendInput(string text) => Input = Input.Length == 0 ? text : Input + text;

    public void LoadIntoInput(string text) => Input = text;

    public void Announce(string text, bool error = false) => AddNote(text, error);

    internal List<FileAttachment> TakeAttachments()
    {
        var attachments = Attachments.Select(a => new FileAttachment(a.FileName, a.Mime, a.Data)).ToList();
        Attachments.Clear();
        return attachments;
    }

    internal void AddNote(string text, bool error = false) => ChatItems.Add(new SystemNoteItem(text, error));

    // ---- SSE transcript building for this session ----

    internal void OnMessageUpdated(JsonElement data)
    {
        if (!data.TryGetProperty("info", out var info))
            return;
        var id = info.TryGetProperty("id", out var i) ? i.GetString() : null;
        var role = info.TryGetProperty("role", out var r) ? r.GetString() : null;
        if (id is null || role is null)
            return;
        _roleByMessage[id] = role;
        if (info.TryGetProperty("time", out var time) && time.TryGetProperty("created", out var c) && c.TryGetInt64(out var ms))
            _createdByMessage[id] = ms;
        if (role != "assistant")
            return;
        if (info.TryGetProperty("providerID", out var pid) && info.TryGetProperty("modelID", out var mid) &&
            pid.GetString() is { Length: > 0 } provider && mid.GetString() is { Length: > 0 } modelName)
        {
            _lastUsedModel = $"{provider}/{modelName}";
            UpdateContextLimit();
        }
        if (ChatParsing.BuildMeta(info) is { } meta)
        {
            _metaByMessage[id] = meta;
            if (_textsByMessage.TryGetValue(id, out var metaItems))
                foreach (var item in metaItems)
                    item.MetaLabel = meta;
        }
        var total = ChatParsing.ReadTokenTotal(info);
        if (total > 0)
        {
            if (_tokensByMessage.TryAdd(id, total))
                _tokenOrder.Add(id);
            else
                _tokensByMessage[id] = total;
            UpdateContextText();
        }
    }

    private void UpdateContextLimit() =>
        _contextLimit = _lastUsedModel is { } key && _owner.TryGetContextLimit(key, out var limit) ? limit : 0;

    private void UpdateContextText()
    {
        var total = _tokenOrder.Count > 0 ? _tokensByMessage[_tokenOrder[^1]] : 0;
        ContextText = total <= 0 ? null
            : _contextLimit > 0
                ? $"Ctx {ChatParsing.FormatPercent(total * 100.0 / _contextLimit)}% · {ChatParsing.FormatTokens(total)} / {ChatParsing.FormatTokens(_contextLimit)}"
                : $"Ctx {ChatParsing.FormatTokens(total)} tok";
    }

    internal void OnPartUpdated(JsonElement data)
    {
        if (!data.TryGetProperty("part", out var part))
            return;
        var id = part.TryGetProperty("id", out var i) ? i.GetString() : null;
        var partType = part.TryGetProperty("type", out var ty) ? ty.GetString() : null;
        if (id is null)
            return;

        // user parts are rendered locally at send time; stream echoes are skipped
        var messageId = part.TryGetProperty("messageID", out var mi) ? mi.GetString() : null;
        if (messageId is not null && _roleByMessage.TryGetValue(messageId, out var role) && role == "user")
            return;

        switch (partType)
        {
            case "text":
                if (!_textByPart.TryGetValue(id, out var textItem))
                {
                    textItem = new AssistantTextItem(CreatedAtFor(messageId)) { MetaLabel = MetaFor(messageId) };
                    _textByPart[id] = textItem;
                    if (messageId is not null)
                    {
                        if (!_textsByMessage.TryGetValue(messageId, out var textList))
                            _textsByMessage[messageId] = textList = [];
                        textList.Add(textItem);
                    }
                    ChatItems.Add(textItem);
                }
                if (part.TryGetProperty("text", out var txt))
                    textItem.Text = txt.GetString() ?? "";
                break;
            case "reasoning":
                if (!_reasoningByPart.TryGetValue(id, out var reasoningItem))
                {
                    reasoningItem = new ReasoningItem(CreatedAtFor(messageId));
                    _reasoningByPart[id] = reasoningItem;
                    ChatItems.Add(reasoningItem);
                }
                if (part.TryGetProperty("text", out var reasoningText))
                    reasoningItem.Text = reasoningText.GetString() ?? "";
                break;
            case "tool":
                if (!_toolByPart.TryGetValue(id, out var toolItem))
                {
                    var toolName = part.TryGetProperty("tool", out var tn) ? tn.GetString() ?? "tool" : "tool";
                    toolItem = new ToolItem(toolName);
                    _toolByPart[id] = toolItem;
                    ChatItems.Add(toolItem);
                }
                if (part.TryGetProperty("state", out var state))
                {
                    ApplyToolState(state, toolItem);
                    ApplyToolMetadata(state, toolItem);
                }
                break;
            case "file":
                if (!_imageByPart.ContainsKey(id) && part.TryGetProperty("url", out var url) &&
                    part.TryGetProperty("filename", out var fn))
                {
                    var item = ImageItem.FromDataUrl(url.GetString(), fn.GetString() ?? "image.png", CreatedAtFor(messageId));
                    if (item is not null)
                    {
                        _imageByPart[id] = item;
                        ChatItems.Add(item);
                    }
                }
                break;
        }
    }

    internal void OnPartRemoved(JsonElement data)
    {
        var id = data.TryGetProperty("partID", out var pid) ? pid.GetString()
            : data.TryGetProperty("part", out var p) && p.TryGetProperty("id", out var pi) ? pi.GetString()
            : null;
        if (id is null)
            return;
        ChatItem? removed = null;
        if (_textByPart.Remove(id, out var textItem))
            removed = textItem;
        else if (_toolByPart.Remove(id, out var toolItem))
            removed = toolItem;
        else if (_imageByPart.Remove(id, out var imageItem))
            removed = imageItem;
        if (removed is null)
            return;
        ChatItems.Remove(removed);
    }

    internal void OnTodosUpdated(JsonElement data)
    {
        Tasks.Clear();
        if (!data.TryGetProperty("todos", out var todos) || todos.ValueKind != JsonValueKind.Array)
            return;
        foreach (var todo in todos.EnumerateArray())
        {
            var content = todo.TryGetProperty("content", out var c) ? c.GetString() : null;
            if (string.IsNullOrEmpty(content))
                continue;
            var status = todo.TryGetProperty("status", out var st) ? st.GetString() ?? "pending" : "pending";
            Tasks.Add(new TaskItem(content, status));
        }
        // live arrivals only — Restore fills Tasks without raising this, so reopening
        // an old session never springs the flyout on its own
        if (Tasks.Count > 0)
            TodosUpdated?.Invoke();
    }

    /// <summary>The tile's view auto-opens the tasks flyout on this signal.</summary>
    internal event Action? TodosUpdated;

    internal void Restore(List<StoredMessage> messages)
    {
        List<TaskItem>? todos = null;
        foreach (var msg in messages)
        {
            _roleByMessage[msg.Id] = msg.Role;
            _createdByMessage[msg.Id] = msg.CreatedMs;
            if (ChatParsing.BuildMeta(msg.Info) is { } meta)
                _metaByMessage[msg.Id] = meta;
            if (msg.Role == "assistant")
            {
                // refill the ctx pill after a chat reopen
                if (msg.Info.TryGetProperty("providerID", out var rp) && msg.Info.TryGetProperty("modelID", out var rm) &&
                    rp.GetString() is { Length: > 0 } provider && rm.GetString() is { Length: > 0 } modelName)
                {
                    _lastUsedModel = $"{provider}/{modelName}";
                    UpdateContextLimit();
                }
                var total = ChatParsing.ReadTokenTotal(msg.Info);
                if (total > 0)
                {
                    if (_tokensByMessage.TryAdd(msg.Id, total))
                        _tokenOrder.Add(msg.Id);
                    else
                        _tokensByMessage[msg.Id] = total;
                }
            }
            var createdAt = ChatParsing.FromMs(msg.CreatedMs);
            foreach (var part in msg.Parts)
            {
                var id = part.TryGetProperty("id", out var pid) ? pid.GetString() : null;
                var partType = part.TryGetProperty("type", out var ty) ? ty.GetString() : null;
                if (id is null || partType is null)
                    continue;
                switch (partType)
                {
                    case "text":
                        var text = part.TryGetProperty("text", out var tx) ? tx.GetString() ?? "" : "";
                        if (msg.Role == "user")
                        {
                            if (text.Length > 0)
                                ChatItems.Add(new UserMessageItem(text, createdAt));
                        }
                        else
                        {
                            var item = new AssistantTextItem(createdAt) { Text = text, MetaLabel = MetaFor(msg.Id) };
                            _textByPart[id] = item;
                            if (!_textsByMessage.TryGetValue(msg.Id, out var restoredTexts))
                                _textsByMessage[msg.Id] = restoredTexts = [];
                            restoredTexts.Add(item);
                            ChatItems.Add(item);
                        }
                        break;
                    case "reasoning":
                        var reasoning = new ReasoningItem(createdAt);
                        if (part.TryGetProperty("text", out var rx))
                            reasoning.Text = rx.GetString() ?? "";
                        _reasoningByPart[id] = reasoning;
                        ChatItems.Add(reasoning);
                        break;
                    case "tool":
                        var toolName = part.TryGetProperty("tool", out var tn) ? tn.GetString() ?? "tool" : "tool";
                        var toolItem = new ToolItem(toolName);
                        if (part.TryGetProperty("state", out var state))
                        {
                            ApplyToolState(state, toolItem);
                            ApplyToolMetadata(state, toolItem);
                        }
                        _toolByPart[id] = toolItem;
                        ChatItems.Add(toolItem);
                        if (toolName == "todowrite")
                            todos = ChatParsing.TodosFromPart(part);
                        break;
                    case "file":
                        if (part.TryGetProperty("url", out var url))
                        {
                            var fileName = part.TryGetProperty("filename", out var fn) ? fn.GetString() ?? "image.png" : "image.png";
                            var item = ImageItem.FromDataUrl(url.GetString(), fileName, createdAt);
                            if (item is not null)
                            {
                                _imageByPart[id] = item;
                                ChatItems.Add(item);
                            }
                            else
                            {
                                ChatItems.Add(new SystemNoteItem($"Attachment: {fileName}"));
                            }
                        }
                        break;
                }
            }
        }
        if (todos is not null)
            foreach (var t in todos)
                Tasks.Add(t);
        UpdateContextText();
    }

    private void ApplyToolState(JsonElement state, ToolItem toolItem)
    {
        if (state.TryGetProperty("status", out var s) && s.GetString() is { } status)
        {
            toolItem.Status = status;
            toolItem.IsError = status.Equals("error", StringComparison.OrdinalIgnoreCase);
        }
        if (state.TryGetProperty("title", out var ti) && ti.GetString() is { Length: > 0 } title)
            toolItem.Title = title;
        var input = ChatParsing.DescribePart(state, "input");
        var output = ChatParsing.DescribePart(state, "output");
        var error = ChatParsing.DescribePart(state, "error");
        toolItem.ErrorPreview = error.Length > 0 ? error.Split('\n', 2)[0] : "";
        toolItem.Details = ChatParsing.BuildToolDetails(input, output, error);
    }

    private void ApplyToolMetadata(JsonElement state, ToolItem toolItem)
    {
        if (!state.TryGetProperty("metadata", out var md) || md.ValueKind != JsonValueKind.Object)
            return;
        if (md.TryGetProperty("filepath", out var fp) && fp.GetString() is { Length: > 0 } filePath)
            toolItem.FilePath = filePath;
        if (md.TryGetProperty("filediff", out var fd) && fd.ValueKind == JsonValueKind.Object)
        {
            toolItem.FileDiff = fd.TryGetProperty("patch", out var patch) ? patch.GetString() : null;
            toolItem.Additions = fd.TryGetProperty("additions", out var add) && add.TryGetInt32(out var a) ? a : 0;
            toolItem.Deletions = fd.TryGetProperty("deletions", out var del) && del.TryGetInt32(out var d) ? d : 0;
        }
    }

    private DateTime? CreatedAtFor(string? messageId) =>
        messageId is not null && _createdByMessage.TryGetValue(messageId, out var ms) && ms > 0
            ? ChatParsing.FromMs(ms)
            : null;

    private string MetaFor(string? messageId) =>
        messageId is not null && _metaByMessage.TryGetValue(messageId, out var meta) ? meta : "";
}

/// <summary>Shared JSON-shape parsing used by the transcript builders.</summary>
internal static class ChatParsing
{
    public static string FormatTokens(long tokens)
    {
        if (tokens < 1000)
            return tokens.ToString();
        if (tokens < 1_000_000)
            return (tokens / 1000d).ToString("0.#") + "k";
        return (tokens / 1_000_000d).ToString("0.##") + "M";
    }

    public static DateTime? FromMs(long ms) =>
        ms > 0 ? DateTimeOffset.FromUnixTimeMilliseconds(ms).ToLocalTime().DateTime : null;

    /// <summary>Keeps one decimal below 10 so small fills don't round down to a bare 0%.</summary>
    public static string FormatPercent(double value) =>
        value >= 10 ? value.ToString("F0") : value.ToString("0.#");

    public static string FormatDuration(TimeSpan elapsed) =>
        elapsed.TotalSeconds < 1 ? $"{elapsed.TotalMilliseconds:F0} ms"
        : elapsed.TotalSeconds < 60 ? $"{elapsed.TotalSeconds:F0}s"
        : $"{(int)elapsed.TotalMinutes}m {elapsed.Seconds:00}s";

    public static long ReadTokenTotal(JsonElement info)
    {
        if (!info.TryGetProperty("tokens", out var tokens) || tokens.ValueKind != JsonValueKind.Object)
            return 0;
        long Sum(string name) =>
            tokens.TryGetProperty(name, out var v) && v.TryGetInt64(out var n) ? n : 0;
        long cacheRead = 0, cacheWrite = 0;
        if (tokens.TryGetProperty("cache", out var cache) && cache.ValueKind == JsonValueKind.Object)
        {
            cacheRead = cache.TryGetProperty("read", out var cr) && cr.TryGetInt64(out var crn) ? crn : 0;
            cacheWrite = cache.TryGetProperty("write", out var cw) && cw.TryGetInt64(out var cwn) ? cwn : 0;
        }
        return Sum("input") + Sum("output") + Sum("reasoning") + cacheRead + cacheWrite;
    }

    /// <summary>Compact per-message detail line: model, token flow, cost.</summary>
    public static string? BuildMeta(JsonElement info)
    {
        if (info.ValueKind != JsonValueKind.Object)
            return null;
        var parts = new List<string>();
        var modelId = info.TryGetProperty("modelID", out var mid) ? mid.GetString() : null;
        var providerId = info.TryGetProperty("providerID", out var pid) ? pid.GetString() : null;
        if (!string.IsNullOrEmpty(modelId))
            parts.Add(!string.IsNullOrEmpty(providerId) ? $"{providerId}/{modelId}" : modelId);
        if (info.TryGetProperty("tokens", out var tokens) && tokens.ValueKind == JsonValueKind.Object)
        {
            var input = tokens.TryGetProperty("input", out var tin) && tin.TryGetInt64(out var inCount) ? inCount : 0;
            var output = tokens.TryGetProperty("output", out var tout) && tout.TryGetInt64(out var outCount) ? outCount : 0;
            if (input > 0 || output > 0)
                parts.Add($"↑{FormatTokens(input)} ↓{FormatTokens(output)}");
        }
        if (info.TryGetProperty("cost", out var cost) && cost.TryGetDouble(out var value) && value > 0)
            parts.Add($"${value:0.####}");
        return parts.Count > 0 ? string.Join(" · ", parts) : null;
    }

    // building these options is expensive (serializer metadata cache); never do it per call
    private static readonly JsonSerializerOptions IndentedJson = new() { WriteIndented = true };

    public static string DescribePart(JsonElement state, string name)
    {
        if (!state.TryGetProperty(name, out var value) || value.ValueKind is JsonValueKind.Null or JsonValueKind.Undefined)
            return "";
        var text = value.ValueKind == JsonValueKind.String
            ? value.GetString() ?? ""
            : JsonSerializer.Serialize(value, IndentedJson);
        if (text.Length == 0)
            return "";
        return text.Length <= 4000 ? text : text[..4000] + "\n… (truncated)";
    }

    public static string BuildToolDetails(string input, string output, string error)
    {
        var sb = new System.Text.StringBuilder();
        if (input.Length > 0)
            sb.Append("input:\n").Append(input).Append('\n');
        if (output.Length > 0)
        {
            if (sb.Length > 0)
                sb.Append('\n');
            sb.Append("output:\n").Append(output).Append('\n');
        }
        if (error.Length > 0)
        {
            if (sb.Length > 0)
                sb.Append('\n');
            sb.Append("error:\n").Append(error).Append('\n');
        }
        return sb.ToString();
    }

    /// <summary>Last todowrite tool part in the restored transcript carries the session's todos.</summary>
    public static List<TaskItem>? TodosFromPart(JsonElement part)
    {
        JsonElement list = default;
        if (part.TryGetProperty("state", out var state))
        {
            if (state.TryGetProperty("input", out var input) &&
                input.TryGetProperty("todos", out var inputTodos) &&
                inputTodos.ValueKind == JsonValueKind.Array)
                list = inputTodos;
            else if (state.TryGetProperty("metadata", out var md) &&
                     md.ValueKind == JsonValueKind.Object &&
                     md.TryGetProperty("todos", out var mdTodos) &&
                     mdTodos.ValueKind == JsonValueKind.Array)
                list = mdTodos;
        }
        if (list.ValueKind != JsonValueKind.Array)
            return null;
        var result = new List<TaskItem>();
        foreach (var todo in list.EnumerateArray())
        {
            var content = todo.TryGetProperty("content", out var c) ? c.GetString() : null;
            if (string.IsNullOrEmpty(content))
                continue;
            var status = todo.TryGetProperty("status", out var st) ? st.GetString() ?? "pending" : "pending";
            result.Add(new TaskItem(content, status));
        }
        return result;
    }

    public static string DeriveTitle(string text)
    {
        var firstLine = text.Split('\n', 2)[0].Trim();
        foreach (var extra in new[] { "\r", "\t" })
            firstLine = firstLine.Replace(extra, " ");
        while (firstLine.Contains("  "))
            firstLine = firstLine.Replace("  ", " ");
        return firstLine.Length <= 48 ? firstLine : firstLine[..48].TrimEnd() + "…";
    }
}
