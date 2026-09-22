using Avalonia;
using Avalonia.Media;
using SukiUI;
using SukiUI.Models;

namespace WarpAgent.Services;

/// <summary>
/// Accent-color presets from the settings page. Each preset re-derives the whole
/// accent family declared in App.xaml — primary, dim tints, on-accent text, and
/// the success colors that share the accent hue — plus SukiUI's own color theme,
/// which paints the surfaces that keep their own color: the titlebar glass shader,
/// text selection (SukiPrimaryColor*) and stock control accents. Danger / warn /
/// add-diff colors stay fixed on purpose: they carry their own semantics.
/// </summary>
public static class AccentTheme
{
    public const string Green = "green";

    public sealed class Preset
    {
        public Preset(string key, string label, uint accent, uint dim, uint on, uint successDim)
        {
            Key = key;
            Label = label;
            Accent = C(accent);
            Dim = C(dim);
            On = C(on);
            SuccessDim = C(successDim);
            SwatchBrush = new SolidColorBrush(Accent);
        }

        public string Key { get; }
        public string Label { get; }
        public Color Accent { get; }
        public Color Dim { get; }
        public Color On { get; }
        public Color SuccessDim { get; }
        public SolidColorBrush SwatchBrush { get; }

        private static Color C(uint rgb) => Color.FromUInt32(0xFF000000 | rgb);
    }

    public static readonly Preset[] Presets =
    {
        // green is the original palette; the grays run light → dark for a mono look
        new("green",    "Green",    0x4AC265, 0x223322, 0x0C140C, 0x1D3122),
        new("amber",    "Amber",    0xE8A348, 0x33270F, 0x171006, 0x382B12),
        new("salmon",   "Salmon",   0xF47067, 0x38201D, 0x1A0B09, 0x3B2523),
        new("gray",     "Gray",     0xA8A8A8, 0x262626, 0x101010, 0x2F2F2F),
        new("silver",   "Silver",   0xC9C9C9, 0x303030, 0x141414, 0x383838),
        new("graphite", "Graphite", 0x7A7A7A, 0x1F1F1F, 0x0E0E0E, 0x292929),
        new("charcoal", "Charcoal", 0x505050, 0x1A1A1A, 0x0C0C0C, 0x232323),
    };

    public static string Normalize(string? accent) =>
        Presets.Any(p => p.Key == accent) ? accent! : Green;

    public static Preset For(string? accent) =>
        Presets.First(p => p.Key == Normalize(accent));

    /// <summary>Swaps the accent-family resources; safe to call before or after App init.</summary>
    public static void Apply(string? accent)
    {
        var resources = Application.Current?.Resources;
        if (resources is null)
            return;
        var p = For(accent);
        resources["AccentBrush"] = new SolidColorBrush(p.Accent);
        resources["AccentDimBrush"] = new SolidColorBrush(p.Dim);
        resources["OnAccentBrush"] = new SolidColorBrush(p.On);
        resources["SuccessBrush"] = new SolidColorBrush(p.Accent);
        resources["SuccessDimBrush"] = new SolidColorBrush(p.SuccessDim);
        // both shader colors get the accent so the glass tint stays uniform instead
        // of mixing two hues; setting this is what recolors selection and Suki defaults
        SukiTheme.GetInstance().ChangeColorTheme(new SukiColorTheme(p.Label, p.Accent, p.Accent));
    }
}
