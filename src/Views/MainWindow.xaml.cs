using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Input.Platform;
using Avalonia.Interactivity;
using Avalonia.Markup.Xaml;
using Avalonia.Platform.Storage;
using Avalonia.VisualTree;
using WarpAgent.Services;
using WarpAgent.ViewModels;
using SukiUI.Controls;

namespace WarpAgent.Views;

public partial class MainWindow : SukiWindow
{
    private const double SidebarMinWidth = 180;
    private const double SidebarMaxWidth = 480;

    private readonly MainViewModel _vm = new();
    private Grid? _layoutRoot;
    private ColumnDefinition? SidebarColumn => _layoutRoot?.ColumnDefinitions[0];

    public MainWindow()
    {
        InitializeComponent();
        _layoutRoot = this.FindControl<Grid>("LayoutRoot");
        DataContext = _vm;
        // the overlay exists from now on: CrashGuard errors land here instead of a queue
        CrashGuard.AttachOverlay(_vm.Error);
        if (SidebarColumn is { } column && _vm.SidebarWidth is { } width)
            column.Width = new GridLength(Math.Clamp(width, SidebarMinWidth, SidebarMaxWidth));
        // the splitter drives column 0 directly and nothing caps it in XAML any more —
        // clamp here so the sidebar and the width persisted on close stay in range
        SidebarColumn?.PropertyChanged += (_, e) =>
        {
            if (e.Property != ColumnDefinition.WidthProperty || SidebarColumn is not { } col || !col.Width.IsAbsolute)
                return;
            var clamped = Math.Clamp(col.Width.Value, SidebarMinWidth, SidebarMaxWidth);
            if (Math.Abs(clamped - col.Width.Value) > 0.5)
                col.Width = new GridLength(clamped);
        };
    }

    private void InitializeComponent() => AvaloniaXamlLoader.Load(this);

    // ---- sidebar rows ----

    private void OnWorkspaceClick(object? sender, TappedEventArgs e)
    {
        if ((sender as Control)?.DataContext is not WorkspaceItem ws)
            return;
        // a tap on the kebab/plus buttons is their own business, not a section toggle
        if (e.Source is Button)
            return;
        _vm.SelectWorkspace(ws);
        // the whole row is the toggle: anywhere-click expands or collapses the section
        var expanding = !ws.IsExpanded;
        ws.IsExpanded = expanding;
        // opening the chat list is intent to work in that workspace
        if (expanding)
            _vm.ConnectExpandedWorkspace(ws);
    }

    private void OnSessionClick(object? sender, TappedEventArgs e)
    {
        if ((sender as Control)?.DataContext is SessionItem session)
            _ = _vm.SelectSessionAsync(session);
    }

    // the "..." button opens the row's own context menu, which carries this button's workspace
    private void OnWorkspaceMenuClick(object? sender, RoutedEventArgs e)
    {
        if (sender is not Control { DataContext: WorkspaceItem } button)
            return;
        if (button.GetVisualAncestors().OfType<Border>().FirstOrDefault(b => b.ContextMenu is not null) is { } row)
            row.ContextMenu!.Open(row);
    }

    private void OnRenameClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is SessionItem session)
            _vm.BeginRename(session);
    }

    private async void OnShareClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is not SessionItem session)
            return;
        var url = await _vm.ShareSessionAsync(session);
        if (url is null)
            return;
        if (TopLevel.GetTopLevel(this)?.Clipboard is { } clipboard)
            await clipboard.SetTextAsync(url);
        _vm.Announce($"Share link copied: {url}");
    }

    private async void OnUnshareClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is SessionItem session)
            await _vm.UnshareSessionAsync(session);
    }

    private void OnPinClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is SessionItem session)
            _vm.TogglePin(session);
    }

    private void OnUnpinClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is SessionItem session)
            _vm.TogglePin(session);
    }

    private void OnDeleteClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is SessionItem session)
            _ = _vm.DeleteSessionCommand.ExecuteAsync(session);
    }

    private void OnRenameKeyDown(object? sender, KeyEventArgs e)
    {
        if ((sender as Control)?.DataContext is not SessionItem session)
            return;
        if (e.Key == Key.Enter)
        {
            e.Handled = true;
            _ = _vm.CommitRenameCommand.ExecuteAsync(session);
        }
        else if (e.Key == Key.Escape)
        {
            e.Handled = true;
            _vm.CancelRename(session);
        }
    }

    private void OnRenameLostFocus(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is SessionItem session && session.IsEditing)
            _ = _vm.CommitRenameCommand.ExecuteAsync(session);
    }

    private async void OnAddWorkspaceClicked(object? sender, RoutedEventArgs e)
    {
        var folders = await StorageProvider.OpenFolderPickerAsync(new FolderPickerOpenOptions
        {
            Title = "Workspace folder for opencode",
            AllowMultiple = false,
        });
        if (folders.Count > 0 && folders[0].Path is { IsAbsoluteUri: true } path)
            _vm.AddWorkspace(path.LocalPath);
    }

    // ---- workspace context menu ----

    private void OnWorkspaceStartClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is WorkspaceItem ws)
            _ = _vm.StartWorkspaceCommand.ExecuteAsync(ws);
    }

    private void OnWorkspaceStopClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is WorkspaceItem ws)
            _vm.StopWorkspaceCommand.Execute(ws);
    }

    private void OnWorkspaceRemoveClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is WorkspaceItem ws)
            _vm.RemoveWorkspace(ws);
    }

    private void OnUnlinkClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is SessionItem session)
            _vm.RemoveFromWorkspace(session);
    }

    private void OnRunningClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is RunningAgentItem running)
            _ = _vm.OpenRunningAsync(running);
    }

    // ---- error overlay ----

    private async void OnCopyErrorClick(object? sender, RoutedEventArgs e)
    {
        var text = _vm.Error.BuildCopyText();
        if (text.Length == 0 || TopLevel.GetTopLevel(this)?.Clipboard is not { } clipboard)
            return;
        await clipboard.SetTextAsync(text);
    }

    private void OnCloseErrorClick(object? sender, RoutedEventArgs e) =>
        _vm.Error.CloseReportCommand.Execute(null);

    protected override void OnOpened(EventArgs e)
    {
        base.OnOpened(e);
        // Avalonia leaves the Wayland app_id empty; the taskbar matches the desktop entry by it
        WaylandAppId.Apply(this, "WarpAgent");
    }

    protected override void OnClosing(WindowClosingEventArgs e)
    {
        if (SidebarColumn is { } column && column.Width.IsAbsolute)
            _vm.SetSidebarWidth(column.Width.Value);
        _vm.Shutdown();
        base.OnClosing(e);
    }
}
