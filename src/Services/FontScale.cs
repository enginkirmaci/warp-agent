using Avalonia;
using Avalonia.Threading;

namespace WarpAgent.Services;

/// <summary>
/// Text-size variants from the settings page ("compact" / "normal" / "large").
/// Swaps the Font* double resources declared in App.xaml — every XAML FontSize
/// binds them via DynamicResource, so a swap restyles the whole UI live;
/// MarkdownTextBlock/DiffTextBlock listen to <see cref="Changed"/> to rebuild
/// text they already rendered from code.
/// </summary>
public static class FontScale
{
    public const string Compact = "compact";
    public const string Normal = "normal";
    public const string Large = "large";

    /// <summary>Raised after the tokens changed; code-built text rebuilds on it.</summary>
    public static event EventHandler? Changed;

    private static string? _current;
    private static double _system = 1.0;

    private static readonly (string Key, double Base)[] Tokens =
    {
        ("FontMicro", 10.5),
        ("FontXSmall", 11),
        ("FontDetail", 11.5),
        ("FontSmall", 12),
        ("FontBody", 12.5),
        ("FontBodyLarge", 13),
        ("FontH1", 18),
        ("FontH2", 16),
        ("FontH3", 14.5),
        ("FontTitle", 19),
        // icon sizes ride the same factor so glyphs grow with the text around them
        ("IconNano", 9),
        ("IconTiny", 10),
        ("IconXSmall", 11),
        ("IconSmall", 12),
        ("IconBody", 13),
        ("IconMedium", 14),
        ("IconLarge", 15),
        ("IconXL", 17),
    };

    public static string Normalize(string? scale) =>
        scale == Compact || scale == Large ? scale : Normal;

    public static void Apply(string? scale)
    {
        _current = scale;
        var resources = Application.Current?.Resources;
        if (resources is null)
            return;
        var factor = Normalize(scale) switch
        {
            Compact => 0.85,
            Large => 1.15,
            _ => 1.0,
        } * _system;
        foreach (var (key, basis) in Tokens)
            resources[key] = Math.Round(basis * factor, 2);
        Changed?.Invoke(null, EventArgs.Empty);
    }

    /// <summary>
    /// Multiplier from the desktop text scaling (<see cref="SystemFontScale"/>); the
    /// in-app compact/normal/large choice rides on top of it. Re-applies the last
    /// setting so live system changes restyle immediately. Safe from any thread.
    /// </summary>
    public static void SetSystemFactor(double factor)
    {
        if (double.IsNaN(factor) || factor <= 0 || Math.Abs(factor - _system) <= 0.001)
            return;
        _system = factor;
        if (_current is null)
            return;
        // the gsettings monitor reports from a worker thread; tokens and the
        // Changed subscribers are UI-affine
        Dispatcher.UIThread.Post(() => Apply(_current));
    }
}
