using System.Diagnostics;
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
    // width bounds live on the VM (same consts feed the persisted clamp); only the
    // splitter column's own DIP width is view-local
    private const double SidebarMinWidth = MainViewModel.SidebarMinWidth;
    private const double SidebarMaxWidth = MainViewModel.SidebarMaxWidth;
    private const double SidebarDefaultWidth = MainViewModel.SidebarDefaultWidth;
    private const double SplitterWidth = 12;

    private readonly MainViewModel _vm = new();
    private Grid? _layoutRoot;
    private ColumnDefinition? SidebarColumn => _layoutRoot?.ColumnDefinitions[0];
    // last absolute sidebar width; zero-width columns mean hidden, this is what Show restores
    private double _sidebarShownWidth = SidebarDefaultWidth;

    public MainWindow()
    {
        InitializeComponent();
        _layoutRoot = this.FindControl<Grid>("LayoutRoot");
        // the vm's ctor already loaded the tooltip preference, but this window is not
        // in desktop.Windows yet — seed it explicitly
        ToolTips.ApplyTo(this);
        DataContext = _vm;
        // the overlay exists from now on: CrashGuard errors land here instead of a queue
        CrashGuard.AttachOverlay(_vm.Error);
        _sidebarShownWidth = Math.Clamp(_vm.SidebarWidth.GetValueOrDefault(SidebarDefaultWidth), SidebarMinWidth, SidebarMaxWidth);
        ApplySidebarLayout();
        // the splitter drives column 0 directly and nothing caps it in XAML any more —
        // clamp here so the sidebar and the width persisted on close stay in range;
        // a zero width is the hidden state and must pass through untouched
        SidebarColumn?.PropertyChanged += (_, e) =>
        {
            if (e.Property != ColumnDefinition.WidthProperty || SidebarColumn is not { } col || !col.Width.IsAbsolute)
                return;
            if (col.Width.Value < 0.5)
                return;
            var clamped = Math.Clamp(col.Width.Value, SidebarMinWidth, SidebarMaxWidth);
            if (Math.Abs(clamped - col.Width.Value) > 0.5)
                col.Width = new GridLength(clamped);
        };
    }

    private void ApplySidebarLayout()
    {
        if (_layoutRoot is not { } root)
            return;
        var cols = root.ColumnDefinitions;
        if (_vm.SidebarVisible)
        {
            cols[0].Width = new GridLength(_sidebarShownWidth);
            cols[1].Width = new GridLength(SplitterWidth);
        }
        else
        {
            cols[0].Width = new GridLength(0);
            cols[1].Width = new GridLength(0);
        }
    }

    private void OnToggleSidebarClick(object? sender, RoutedEventArgs e)
    {
        if (SidebarColumn is not { } col)
        {
            _vm.SetSidebarVisible(!_vm.SidebarVisible);
            ApplySidebarLayout();
            return;
        }
        if (col.Width.IsAbsolute && col.Width.Value >= SidebarMinWidth)
            _sidebarShownWidth = col.Width.Value;
        _vm.SetSidebarVisible(!_vm.SidebarVisible);
        ApplySidebarLayout();
    }

    private void InitializeComponent() => AvaloniaXamlLoader.Load(this);

    // ---- sidebar rows ----

    private void OnWorkspaceClick(object? sender, TappedEventArgs e)
    {
        if ((sender as Control)?.DataContext is not WorkspaceItem ws)
            return;
        // a tap on the kebab/new-chat buttons is their own business, not a section toggle;
        // the glyph inside a button is the gesture source, so walk up the tree to catch it
        if (e.Source is Avalonia.Visual visual && visual.GetVisualAncestors().OfType<Button>().Any())
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

    // file manager on the workspace folder; General is not a folder, so it has no item
    private void OnWorkspaceOpenFolderClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is not WorkspaceItem { IsGeneral: false } ws || !Directory.Exists(ws.FolderPath))
            return;
        using var _ = Process.Start(new ProcessStartInfo(ws.FolderPath) { UseShellExecute = true });
    }

    // the opencode TUI in a terminal at the workspace folder, independent of the served instance
    private void OnWorkspaceOpenInOpenCodeClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is not WorkspaceItem { IsGeneral: false } ws || !Directory.Exists(ws.FolderPath))
            return;
        TerminalLauncher.OpenInOpenCode(ws.FolderPath, _vm.Settings.OpenCodeExecutable);
    }

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
        // a zero column width is the hidden sidebar — keep the width from before it was hidden
        if (SidebarColumn is { } column && column.Width.IsAbsolute && column.Width.Value >= SidebarMinWidth)
            _vm.SetSidebarWidth(column.Width.Value);
        _vm.Shutdown();
        base.OnClosing(e);
    }
}
