using System.Text;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;

namespace WarpAgent.ViewModels;

/// <summary>One caught error: where it came from, the exception, and when.</summary>
public sealed record ErrorReport(string Source, Exception Exception, DateTime Time, bool Fatal);

/// <summary>
/// Backing VM of the error overlay: shows one report at a time, queues the rest,
/// and formats the full report (type, message, stack, inner exceptions) for the
/// clipboard. All methods run on the UI thread.
/// </summary>
public sealed partial class ErrorOverlayViewModel : ObservableObject
{
    private readonly object _gate = new();
    private readonly Queue<ErrorReport> _pending = new();
    private ErrorReport? _current;

    [ObservableProperty] private bool _isVisible;
    [ObservableProperty] private string _source = "";
    [ObservableProperty] private string _errorType = "";
    [ObservableProperty] private string _errorMessage = "";
    [ObservableProperty] private string _errorDetails = "";
    [ObservableProperty] private bool _isFatal;
    [ObservableProperty] private int _moreCount;

    public bool HasMore => MoreCount > 0;

    /// <summary>Raised when the last visible report is dismissed (the fatal path waits on it).</summary>
    public event EventHandler? ReportClosed;

    /// <summary>Queues the report and shows it immediately when the overlay is idle.</summary>
    public void Show(ErrorReport report)
    {
        lock (_gate)
        {
            _pending.Enqueue(report);
            if (_current is not null)
                return;
            _current = report;
        }
        Present(report);
    }

    private void Present(ErrorReport report)
    {
        Source = report.Source;
        IsFatal = report.Fatal;
        ErrorType = report.Exception.GetType().FullName ?? "Exception";
        ErrorMessage = report.Exception.Message.Length > 0 ? report.Exception.Message : ErrorType;
        ErrorDetails = BuildDetails(report);
        lock (_gate)
            MoreCount = _pending.Count;
        OnPropertyChanged(nameof(HasMore));
        IsVisible = true;
    }

    [RelayCommand]
    private void CloseReport()
    {
        IsVisible = false;
        ErrorReport? next;
        lock (_gate)
        {
            _current = null;
            next = _pending.Count > 0 ? _pending.Dequeue() : null;
            _current = next;
        }
        if (next is { } followUp)
            Present(followUp);
        else
            ReportClosed?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>The visible report formatted for the clipboard.</summary>
    public string BuildCopyText() => _current is { } current ? Format(current) : "";

    private static string Format(ErrorReport report)
    {
        var sb = new StringBuilder("WarpAgent error report");
        sb.AppendLine().Append("time:   ").AppendLine(report.Time.ToString("yyyy-MM-dd HH:mm:ss"));
        sb.Append("source: ").AppendLine(report.Source);
        if (report.Fatal)
            sb.AppendLine("fatal:  the app cannot continue after this error");
        var ex = report.Exception;
        for (var depth = 0; ex is not null; ex = ex.InnerException, depth++)
        {
            sb.AppendLine().Append(depth == 0 ? "error:  " : $"inner {depth}: ").AppendLine(ex.GetType().FullName);
            sb.AppendLine(ex.Message);
            sb.AppendLine("stack:");
            sb.AppendLine(ex.StackTrace ?? "    (not available)");
        }
        return sb.ToString();
    }

    private static string BuildDetails(ErrorReport report)
    {
        // everything the headline does not already show: time, exception chain, stacks
        var sb = new StringBuilder();
        sb.AppendLine(report.Time.ToString("yyyy-MM-dd HH:mm:ss"));
        var ex = report.Exception;
        for (var depth = 0; ex is not null; ex = ex.InnerException, depth++)
        {
            if (depth > 0)
                sb.AppendLine();
            sb.Append(depth == 0 ? "" : $"inner {depth}: ");
            sb.AppendLine(ex.GetType().FullName ?? "Exception");
            sb.AppendLine(ex.StackTrace ?? "    (no stack trace)");
        }
        return sb.ToString();
    }
}
