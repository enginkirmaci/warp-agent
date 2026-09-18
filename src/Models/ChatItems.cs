using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

namespace WarpAgent.Models;

public abstract class ChatItem : ObservableObject;

public sealed class UserMessageItem(string text, DateTime? createdAt = null) : ChatItem
{
    public string Text { get; } = text;
    public DateTime? CreatedAt { get; } = createdAt;
    public string TimeLabel => TimeLabels.Message(CreatedAt);
}

/// <summary>A message parked while a turn is still running; sent when the session goes idle.
/// Rows offer steer (send next, interrupting the turn), edit (back to composer) and remove.</summary>
public sealed partial class QueuedMessageItem(string text, int imageCount) : ChatItem
{
    public string Text { get; } = text;
    public int ImageCount { get; } = imageCount;
    public bool HasImages => ImageCount > 0;
    public required Action<QueuedMessageItem> Remove { get; init; }
    public required Action<QueuedMessageItem> Steer { get; init; }
    public required Action<QueuedMessageItem> Edit { get; init; }

    [RelayCommand]
    private void RemoveSelf() => Remove(this);

    [RelayCommand]
    private void SteerSelf() => Steer(this);

    [RelayCommand]
    private void EditSelf() => Edit(this);
}

public sealed class AssistantTextItem(DateTime? createdAt = null) : ChatItem
{
    private string _text = "";
    private string _metaLabel = "";

    public DateTime? CreatedAt { get; } = createdAt;

    public string Text { get => _text; set => SetProperty(ref _text, value); }

    /// <summary>Model · tokens · cost of the owning assistant message; empty until known.</summary>
    public string MetaLabel { get => _metaLabel; set => SetProperty(ref _metaLabel, value); }

    public string TimeLabel => TimeLabels.Message(CreatedAt);
}

public sealed class ReasoningItem(DateTime? createdAt = null) : ChatItem
{
    private string _text = "";

    public DateTime? CreatedAt { get; } = createdAt;

    public string Text { get => _text; set => SetProperty(ref _text, value); }

    public string TimeLabel => TimeLabels.Message(CreatedAt);
}

public sealed class ToolItem(string tool) : ChatItem
{
    private string _status = "pending";
    private string _title = tool;
    private string _filePath = "";
    private bool _isError;
    private string _errorPreview = "";
    private string _details = "";
    private string? _fileDiff;
    private int _additions;
    private int _deletions;

    public string Tool { get; } = tool;

    public string Status
    {
        get => _status;
        set
        {
            if (SetProperty(ref _status, value))
            {
                OnPropertyChanged(nameof(IsRunning));
                OnPropertyChanged(nameof(IsCompleted));
            }
        }
    }

    public bool IsRunning => Status == "running";
    public bool IsCompleted => Status == "completed";

    public string Title { get => _title; set => SetProperty(ref _title, value); }

    /// <summary>Touched file path for edit/write tools, from state metadata.</summary>
    public string FilePath { get => _filePath; set => SetProperty(ref _filePath, value); }

    /// <summary>Unified diff for edit tools; null for write/other tools.</summary>
    public string? FileDiff
    {
        get => _fileDiff;
        set
        {
            if (SetProperty(ref _fileDiff, value))
                OnPropertyChanged(nameof(HasDiff));
        }
    }

    public bool HasDiff => FileDiff is { Length: > 0 };

    public int Additions
    {
        get => _additions;
        set
        {
            if (SetProperty(ref _additions, value))
                OnPropertyChanged(nameof(HasNumstat));
        }
    }

    public int Deletions
    {
        get => _deletions;
        set
        {
            if (SetProperty(ref _deletions, value))
                OnPropertyChanged(nameof(HasNumstat));
        }
    }

    public bool HasNumstat => Additions > 0 || Deletions > 0;

    public bool IsError { get => _isError; set => SetProperty(ref _isError, value); }

    /// <summary>First line of the tool error for the row itself; the full text lives in Details.</summary>
    public string ErrorPreview { get => _errorPreview; set => SetProperty(ref _errorPreview, value); }

    public string Details
    {
        get => _details;
        set
        {
            if (SetProperty(ref _details, value))
                OnPropertyChanged(nameof(HasDetails));
        }
    }

    public bool HasDetails => Details.Length > 0;
}

public sealed class SystemNoteItem(string text, bool isError = false) : ChatItem
{
    public string Text { get; } = text;
    public bool IsError { get; } = isError;
}

public sealed class TaskItem(string content, string status)
{
    public string Content { get; } = content;
    public string Status { get; } = status;

    public bool Pending => Status is not ("in_progress" or "completed" or "cancelled");
    public bool InProgress => Status == "in_progress";
    public bool Completed => Status == "completed";
    public bool Cancelled => Status == "cancelled";

    public bool Done => Completed || Cancelled;
}

public partial class PermissionItem : ChatItem
{
    private string? _answer;

    public required string Id { get; init; }
    public required string SessionId { get; init; }
    public required string Kind { get; init; }
    public required string Detail { get; init; }
    public required bool IsV2 { get; init; }
    public required Func<PermissionItem, string, bool, Task> Respond { get; init; }

    /// <summary>Unified diff the ask wants to apply (edit asks only); shown expandable.</summary>
    public string? Diff { get; init; }

    public bool HasDiff => Diff is { Length: > 0 };

    /// <summary>True when the reply was sent by auto-allow rather than a button click.</summary>
    public bool WasAutoAnswered { get; private set; }

    public string Title => $"Permission required — {Kind}";
    public string DetailPreview => Detail.Split('\n', 2)[0];
    public bool IsPending => _answer is null;

    public string? Answer
    {
        get => _answer;
        private set
        {
            if (SetProperty(ref _answer, value))
                OnPropertyChanged(nameof(IsPending));
        }
    }

    public void SetAnswer(string label, bool auto)
    {
        WasAutoAnswered = auto;
        Answer = label;
    }

    [RelayCommand]
    private Task Allow() => Respond(this, "once", false);

    [RelayCommand]
    private Task AllowAlways() => Respond(this, "always", false);

    [RelayCommand]
    private Task Deny() => Respond(this, "reject", false);
}

public static class TimeLabels
{
    public static string Message(DateTime? at)
    {
        if (at is not { } value)
            return "";
        return value.Date == DateTime.Today
            ? value.ToString("HH:mm")
            : value.ToString("dd MMM HH:mm");
    }

    public static string Relative(DateTime at)
    {
        var span = DateTime.Now - at;
        if (span.TotalMinutes < 1)
            return "now";
        if (span.TotalHours < 1)
            return $"{(int)span.TotalMinutes}m";
        if (span.TotalDays < 1)
            return $"{(int)span.TotalHours}h";
        if (span.TotalDays < 7)
            return $"{(int)span.TotalDays}d";
        return at.ToString("yyyy-MM-dd");
    }
}
