using System.Diagnostics;
using System.IO;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Input.Platform;
using Avalonia.Interactivity;
using Avalonia.Markup.Xaml;
using Avalonia.Media.Imaging;
using Avalonia.Platform.Storage;
using WarpAgent.Models;
using WarpAgent.ViewModels;

namespace WarpAgent.Views;

public partial class ChatTile : UserControl
{
    private ScrollViewer? _chatScroll;
    private Button? _jumpLatest;
    private Button? _tasksButton;
    private TextBox? _input;
    // follow-the-latest intent, not a position: recomputing it from the scroll position
    // while content grows is what killed autoscroll right after sending a prompt
    private bool _pinned = true;
    // last on-screen spot of the tasks button; a change while its flyout is open means
    // the grid reflowed and the popup would sit orphaned over some other tile
    private Point _tasksButtonSpot;

    public ChatTile()
    {
        InitializeComponent();
        _chatScroll = this.FindControl<ScrollViewer>("ChatScroll");
        _jumpLatest = this.FindControl<Button>("JumpLatest");
        _tasksButton = this.FindControl<Button>("TasksButton");
        _input = this.FindControl<TextBox>("InputBox");
        // an AcceptsReturn TextBox consumes Enter before bubbling; tunnel to send first
        _input?.AddHandler(InputElement.KeyDownEvent, OnInputKeyDown, RoutingStrategies.Tunnel);
        _input?.GotFocus += (_, _) => Vm?.Focus();
        // sending a prompt re-pins the view to the latest message
        this.FindControl<Button>("SendButton")?.Click += (_, _) => PinToLatest();
        if (_chatScroll is not null)
            _chatScroll.ScrollChanged += OnChatScrollChanged;
        LayoutUpdated += OnLayoutUpdated;
    }

    private void OnLayoutUpdated(object? sender, EventArgs e)
    {
        if (_tasksButton is not { } btn || btn.Flyout is not Flyout { IsOpen: true } flyout)
            return;
        var root = TopLevel.GetTopLevel(btn);
        if (root is null)
            return;
        var spot = btn.TranslatePoint(new Point(0, 0), root);
        if (spot is { } p && _tasksButtonSpot != default && p != _tasksButtonSpot)
            flyout.Hide();
        if (spot is { } q)
            _tasksButtonSpot = q;
    }

    private ChatTileViewModel? Vm => DataContext as ChatTileViewModel;

    private void InitializeComponent() => AvaloniaXamlLoader.Load(this);

    // ScrollChanged carries the post-layout extent, so pinning is decided entirely here.
    // Content growth re-sticks while pinned; only genuine offset moves (user scrolling,
    // no growth) may unpin — position alone can't decide, because right after a send
    // the offset lags the already-grown extent in the same event.
    private void OnChatScrollChanged(object? sender, ScrollChangedEventArgs e)
    {
        if (_chatScroll is null || Vm is not { } vm)
            return;
        var userScrolledUp = e.OffsetDelta.Y < -0.5;
        if (e.ExtentDelta.Y > 0.5 && _pinned && !userScrolledUp)
        {
            _chatScroll.ScrollToEnd();
        }
        else if (e.ExtentDelta.Y <= 0.5 || userScrolledUp)
        {
            var atBottom = _chatScroll.Offset.Y + _chatScroll.Viewport.Height >= _chatScroll.Extent.Height - 1;
            _pinned = atBottom;
        }
        if (_jumpLatest is not null)
            _jumpLatest.IsVisible = !_pinned && vm.HasMessages;
    }

    private void OnJumpLatestClicked(object? sender, RoutedEventArgs e)
    {
        PinToLatest();
    }

    private void PinToLatest()
    {
        _pinned = true;
        _chatScroll?.ScrollToEnd();
        if (_jumpLatest is not null)
            _jumpLatest.IsVisible = false;
    }

    private void OnTileTapped(object? sender, TappedEventArgs e) => Vm?.Focus();

    private async void OnAttachClick(object? sender, RoutedEventArgs e)
    {
        if (Vm is null || TopLevel.GetTopLevel(this) is not { } topLevel)
            return;
        var files = await topLevel.StorageProvider.OpenFilePickerAsync(new FilePickerOpenOptions
        {
            Title = "Attach files",
            AllowMultiple = true,
        });
        foreach (var file in files)
        {
            if (file.Path.LocalPath is not { } path)
                continue;
            var mime = Path.GetExtension(path).ToLowerInvariant() switch
            {
                ".png" => "image/png",
                ".jpg" or ".jpeg" => "image/jpeg",
                ".gif" => "image/gif",
                ".webp" => "image/webp",
                _ => "application/octet-stream",
            };
            Vm.AttachFile(Path.GetFileName(path), mime, await File.ReadAllBytesAsync(path));
        }
    }

    private void OnModelPillClick(object? sender, RoutedEventArgs e) =>
        ShowPicker(sender, Vm?.Owner.Models, vm => vm.SelectedModel = MainViewModel.DefaultModelEntry, (vm, v) => vm.SelectedModel = v);

    private void OnVariantPillClick(object? sender, RoutedEventArgs e) =>
        ShowPicker(sender, Vm?.Variants, vm => vm.SelectedVariant = MainViewModel.DefaultVariantEntry, (vm, v) => vm.SelectedVariant = v);

    private void OnAgentPillClick(object? sender, RoutedEventArgs e) =>
        ShowPicker(sender, Vm?.Owner.Agents, vm => vm.SelectedAgent = MainViewModel.DefaultAgentEntry, (vm, v) => vm.SelectedAgent = v);

    /// <summary>Menu items are built in code: Click handlers declared in a MenuFlyout
    /// ItemTemplate silently never fire (verified), so selection must be wired here.</summary>
    private void ShowPicker(
        object? sender,
        System.Collections.IEnumerable? items,
        Action<ChatTileViewModel> clear,
        Action<ChatTileViewModel, string> select)
    {
        if (Vm is not { } vm || sender is not Button button || items is null)
            return;
        var flyout = new MenuFlyout();
        foreach (var item in items)
        {
            var text = item as string ?? string.Empty;
            var menuItem = new MenuItem { Header = text };
            menuItem.Click += (_, _) =>
            {
                // the sentinel entries mean "no override"; anything else sets the value
                if (text.StartsWith('('))
                    clear(vm);
                else
                    select(vm, text);
            };
            flyout.Items.Add(menuItem);
        }
        button.Flyout = flyout;
        flyout.ShowAt(button);
    }

    private void OnInputKeyDown(object? sender, KeyEventArgs e)
    {
        if (e.Key == Key.V && e.KeyModifiers.HasFlag(KeyModifiers.Control))
        {
            // let the TextBox's own text paste run; an image clipboard no-ops in it
            _ = PasteFromClipboardAsync();
            return;
        }
        if (_input is { } box && (e.Key == Key.Up || e.Key == Key.Down))
        {
            // history browsing only from the first (Up) / last (Down) line, so
            // Shift+Enter multiline drafts keep their caret movement
            var text = box.Text ?? "";
            var caret = box.CaretIndex;
            var boundary = e.Key == Key.Up
                ? text.IndexOf('\n', 0, caret) < 0
                : text.IndexOf('\n', caret) < 0;
            if (boundary && Vm?.CyclePromptHistory(up: e.Key == Key.Up) == true)
            {
                e.Handled = true;
                box.CaretIndex = box.Text?.Length ?? 0;
            }
            return;
        }
        if (e.Key != Key.Enter || e.KeyModifiers.HasFlag(KeyModifiers.Shift))
            return;
        e.Handled = true;
        Vm?.SendCommand.Execute(null);
        PinToLatest();
    }

    /// <summary>Ctrl+V: attach a clipboard image; plain text was already pasted by the TextBox itself.</summary>
    private async Task PasteFromClipboardAsync()
    {
        if (TopLevel.GetTopLevel(this)?.Clipboard is not { } clipboard)
            return;
        try
        {
            var bitmap = await clipboard.TryGetBitmapAsync();
            if (bitmap is not null)
            {
                using var ms = new MemoryStream();
                bitmap.Save(ms, new PngBitmapEncoderOptions());
                Vm?.AttachImage(ms.ToArray());
                return;
            }
            var file = await clipboard.TryGetFileAsync();
            if (file is IStorageFile storageFile && storageFile.Name is { } name &&
                Path.GetExtension(name) is { Length: > 0 } ext &&
                new[] { ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp" }.Contains(ext, StringComparer.OrdinalIgnoreCase))
            {
                await using var stream = await storageFile.OpenReadAsync();
                using var decoded = new Bitmap(stream);
                using var png = new MemoryStream();
                decoded.Save(png, new PngBitmapEncoderOptions());
                Vm?.AttachImage(png.ToArray());
            }
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"[paste] {ex.Message}");
        }
    }

    private async void OnCopyMessage(object? sender, RoutedEventArgs e)
    {
        var text = (sender as Control)?.DataContext switch
        {
            UserMessageItem u => u.Text,
            AssistantTextItem a => a.Text,
            ReasoningItem r => r.Text,
            SystemNoteItem note => note.Text,
            ToolItem t => string.Join(" ", new[] { t.Tool, t.Title }.Where(s => s.Length > 0)),
            PermissionItem p => $"{p.Title}\n{p.Detail}",
            _ => null,
        };
        if (sender is Control { DataContext: ImageItem image } && TopLevel.GetTopLevel(this) is { Clipboard: { } cb2 })
        {
            await cb2.SetBitmapAsync(image.Preview);
            return;
        }
        if (string.IsNullOrEmpty(text) || TopLevel.GetTopLevel(this) is not { Clipboard: { } clipboard })
            return;
        await clipboard.SetTextAsync(text);
    }

    // ---- file drag & drop onto the composer ----

    private void OnFileDragOver(object? sender, DragEventArgs e)
    {
        if (!e.DataTransfer.Contains(DataFormat.File))
            return;
        e.DragEffects = DragDropEffects.Copy;
        e.Handled = true;
    }

    private async void OnFileDrop(object? sender, DragEventArgs e)
    {
        if (!e.DataTransfer.Contains(DataFormat.File))
            return;
        var files = e.DataTransfer.TryGetFiles();
        if (files is null || files.Length == 0)
            return;
        e.Handled = true;
        foreach (var file in files)
            await AttachDroppedFileAsync(file);
    }

    private async Task AttachDroppedFileAsync(IStorageItem item)
    {
        if (item is not IStorageFile file || Vm is not { } vm)
            return;
        try
        {
            await using var stream = await file.OpenReadAsync();
            using var ms = new MemoryStream();
            await stream.CopyToAsync(ms);
            var data = ms.ToArray();
            if (data.Length > 512 * 1024)
            {
                vm.Announce($"{file.Name} is over 512 KB — skipped.", error: true);
                return;
            }
            vm.AttachFile(file.Name, MimeFor(Path.GetExtension(file.Name)), data);
        }
        catch (Exception ex)
        {
            vm.Announce($"Could not attach {file.Name}: {ex.Message}", error: true);
        }
    }

    private static string MimeFor(string ext) => ext.ToLowerInvariant() switch
    {
        ".png" => "image/png",
        ".jpg" or ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".bmp" => "image/bmp",
        ".pdf" => "application/pdf",
        _ => "text/plain",
    };

    // ---- transcript helpers ----

    private void OnEditMessageClick(object? sender, RoutedEventArgs e)
    {
        if ((sender as Control)?.DataContext is not UserMessageItem message)
            return;
        Vm?.LoadIntoInput(message.Text);
        this.FindControl<TextBox>("InputBox")?.Focus();
    }

    private void OnRunningClick(object? sender, RoutedEventArgs e)
    {
        if (Vm is not { } vm || (sender as Control)?.DataContext is not RunningAgentItem running)
            return;
        _ = vm.Owner.OpenRunningAsync(running);
    }
}
