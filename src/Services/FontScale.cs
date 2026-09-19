using Avalonia;

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
    };

    public static string Normalize(string? scale) =>
        scale == Compact || scale == Large ? scale : Normal;

    public static void Apply(string? scale)
    {
        var resources = Application.Current?.Resources;
        if (resources is null)
            return;
        var factor = Normalize(scale) switch
        {
            Compact => 0.92,
            Large => 1.08,
            _ => 1.0,
        };
        foreach (var (key, basis) in Tokens)
            resources[key] = Math.Round(basis * factor, 2);
        Changed?.Invoke(null, EventArgs.Empty);
    }
}
