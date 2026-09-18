using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Input.Platform;
using Avalonia.Interactivity;
using Avalonia.Markup.Xaml;
using Avalonia.Platform.Storage;
using Avalonia.VisualTree;
using WarpAgent.ViewModels;
using SukiUI.Controls;

namespace WarpAgent.Views;

public partial class MainWindow : SukiWindow
{
    private readonly MainViewModel _vm = new();

    public MainWindow()
    {
        InitializeComponent();
        DataContext = _vm;
    }

    private void InitializeComponent() => AvaloniaXamlLoader.Load(this);

    // ---- sidebar rows ----

    private void OnWorkspaceClick(object? sender, TappedEventArgs e)
    {
        if ((sender as Control)?.DataContext is WorkspaceItem ws)
            _vm.SelectWorkspace(ws);
    }

    private void OnWorkspaceCollapseClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is WorkspaceItem ws)
        {
            e.Handled = true;
            ws.IsExpanded = !ws.IsExpanded;
        }
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

    protected override void OnClosing(WindowClosingEventArgs e)
    {
        _vm.Shutdown();
        base.OnClosing(e);
    }
}
