using System.Collections.ObjectModel;
using System.IO;
using CommunityToolkit.Mvvm.ComponentModel;
using WarpAgent.Services;

namespace WarpAgent.ViewModels;

/// <summary>One added workspace folder; owns its own server instance and chat list.</summary>
public sealed class WorkspaceItem : ObservableObject
{
    private bool _isRunning;
    private bool _isSelected;

    public WorkspaceItem(WorkspaceState state)
    {
        State = state;
        Name = state.Path == AppPaths.GeneralWorkspacePath
            ? "General"
            : Path.GetFileName(state.Path.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar)) is { Length: > 0 } name
                ? name
                : state.Path;
        Chats.CollectionChanged += (_, _) =>
        {
            OnPropertyChanged(nameof(HasChats));
            OnPropertyChanged(nameof(ShowEmptyHint));
        };
    }

    public WorkspaceState State { get; }

    /// <summary>Normalized absolute path; also the runtime dictionary key.</summary>
    public string FolderPath => State.Path;

    /// <summary>The built-in general-purpose section: not a project folder, not removable.</summary>
    public bool IsGeneral => FolderPath == AppPaths.GeneralWorkspacePath;

    public string Name { get; }

    /// <summary>Row tooltip: the folder for real workspaces, a hint for General.</summary>
    public string ToolTipText => IsGeneral ? "General chat — not tied to a folder" : FolderPath;

    public bool IsRunning
    {
        get => _isRunning;
        set
        {
            if (SetProperty(ref _isRunning, value))
                OnPropertyChanged(nameof(ShowEmptyHint));
        }
    }

    /// <summary>Chats shown nested under this workspace; filled while its server runs.</summary>
    public ObservableCollection<SessionItem> Chats { get; } = [];

    public bool HasChats => Chats.Count > 0;

    /// <summary>Only a running workspace with no chats shows the muted placeholder.</summary>
    public bool ShowEmptyHint => IsRunning && Chats.Count == 0;

    public bool IsSelected { get => _isSelected; set => SetProperty(ref _isSelected, value); }

    /// <summary>Persisted sidebar expanded state; the chevron flips it.</summary>
    public bool IsExpanded
    {
        get => !State.Collapsed;
        set
        {
            if (IsExpanded == value)
                return;
            State.Collapsed = !value;
            OnPropertyChanged();
            OnPropertyChanged(nameof(ShowOpenFolder));
            OnPropertyChanged(nameof(ShowClosedFolder));
            StateChanged?.Invoke();
        }
    }

    /// <summary>Folder glyphs only fit real folders; General renders the chat glyph instead.</summary>
    public bool ShowOpenFolder => !IsGeneral && IsExpanded;
    public bool ShowClosedFolder => !IsGeneral && !IsExpanded;

    /// <summary>Raised when a persisted field changes; MainViewModel hooks its save.</summary>
    public Action? StateChanged { get; set; }
}
