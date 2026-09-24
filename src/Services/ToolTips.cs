using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;

namespace WarpAgent.Services;

/// <summary>
/// Tooltip visibility from the settings page. ToolTip.ServiceEnabled is an inheriting
/// attached property, so a single set on the window covers every control and popup —
/// no ToolTip.Tip value needs to be touched.
/// </summary>
public static class ToolTips
{
    private static bool _visible = true;

    public static void Apply(bool visible)
    {
        _visible = visible;
        if (Application.Current?.ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
            foreach (var window in desktop.Windows)
                ToolTip.SetServiceEnabled(window, visible);
    }

    /// <summary>Seeds a freshly built window; it is not in desktop.Windows yet during its ctor.</summary>
    public static void ApplyTo(Window window) => ToolTip.SetServiceEnabled(window, _visible);
}
