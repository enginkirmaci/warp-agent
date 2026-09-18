using System.Threading;
using Avalonia.Threading;
using Serilog;
using WarpAgent.ViewModels;

namespace WarpAgent.Services;

/// <summary>
/// The error net: no managed exception may take the app down — a crash stops every
/// running session. UI-thread faults (async void handlers, Dispatcher.Post closures)
/// and unobserved task faults are shown in the error overlay and swallowed, so only
/// the failing operation is lost. A raw background-thread failure is terminal no
/// matter what; there the dialog blocks until it has been read, then the process exits.
/// </summary>
public static class CrashGuard
{
    private static readonly object Gate = new();
    private static readonly Queue<ErrorReport> Pending = new();
    private static readonly ILogger Log = Serilog.Log.ForContext("SourceContext", "CrashGuard");
    private static ErrorOverlayViewModel? _overlay;
    private static bool _installed;

    /// <summary>Wires the three global handlers. Runs before the window exists, so errors
    /// raised before the overlay attaches are queued and replayed once it does.</summary>
    public static void Install()
    {
        if (_installed)
            return;
        _installed = true;
        // async void handlers, Dispatcher.Post closures, control event handlers:
        // Handled=true keeps the message loop (and every session) alive
        Dispatcher.UIThread.UnhandledException += (_, e) =>
        {
            e.Handled = true;
            Report(e.Exception, "ui thread");
        };
        // the many fire-and-forget tasks (`_ = …`) are never observed; observe them here
        TaskScheduler.UnobservedTaskException += (_, e) =>
        {
            e.SetObserved();
            Report(e.Exception, "unobserved task");
        };
        // a raw background-thread failure kills the process when this handler returns
        AppDomain.CurrentDomain.UnhandledException += (_, e) => OnFatal(e.ExceptionObject);
    }

    /// <summary>Connects the overlay once MainWindow is up; queued pre-window errors replay now.</summary>
    public static void AttachOverlay(ErrorOverlayViewModel overlay)
    {
        ErrorReport[] replay;
        lock (Gate)
        {
            _overlay = overlay;
            replay = [.. Pending];
            Pending.Clear();
        }
        foreach (var report in replay)
            overlay.Show(report);
    }

    public static void Report(Exception exception, string source)
    {
        Log.Error(exception, "unhandled error ({Source})", source);
        var report = new ErrorReport(source, exception, DateTime.Now, Fatal: false);
        lock (Gate)
        {
            if (_overlay is null)
            {
                Pending.Enqueue(report);
                return;
            }
        }
        RunOnUi(() => _overlay!.Show(report));
    }

    private static void OnFatal(object? exceptionObject)
    {
        var exception = exceptionObject as Exception ??
                        new Exception(exceptionObject?.ToString() ?? "unknown non-exception failure");
        Log.Fatal(exception, "fatal error — the process cannot survive this");
        // the overlay renders on the UI thread: a UI-thread crash cannot show it, and
        // blocking that thread here would deadlock waiting for its own dismissal
        if (_overlay is null || Dispatcher.UIThread.CheckAccess())
        {
            AppLogging.Shutdown();
            Environment.Exit(2);
        }
        using var shown = new ManualResetEventSlim(false);
        EventHandler closed = (_, _) => shown.Set();
        RunOnUi(() =>
        {
            _overlay.ReportClosed += closed;
            _overlay.Show(new ErrorReport("fatal", exception, DateTime.Now, Fatal: true));
        });
        // block the crashing thread: the CLR exits the process the moment this returns,
        // so hold it open while the dialog is on screen
        shown.Wait(TimeSpan.FromMinutes(10));
        AppLogging.Shutdown();
        Environment.Exit(2);
    }

    private static void RunOnUi(Action action)
    {
        try
        {
            _ = Dispatcher.UIThread.InvokeAsync(() =>
            {
                try
                {
                    action();
                }
                catch (Exception ex)
                {
                    Log.Error(ex, "error overlay failed");
                }
            });
        }
        catch (Exception ex)
        {
            Log.Error(ex, "could not reach the ui thread");
        }
    }
}
