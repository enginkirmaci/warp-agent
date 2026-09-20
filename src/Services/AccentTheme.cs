using Avalonia;
using Avalonia.Media;

namespace WarpAgent.Services;

/// <summary>
/// Accent-color presets from the settings page. Each preset re-derives the whole
/// accent family declared in App.xaml — primary, dim tints, on-accent text, the
/// success colors that share the accent hue, and SukiUI's ThemeAccent ramp — so
/// swapping one recolors every accent surface live via DynamicResource. Danger /
/// warn / add-diff colors stay fixed on purpose: they carry their own semantics.
/// </summary>
public static class AccentTheme
{
    public const string Green = "green";

    public sealed class Preset
    {
        public Preset(string key, string label, uint accent, uint dim, uint on, uint successDim, uint r1, uint r2, uint r4)
        {
            Key = key;
            Label = label;
            Accent = C(accent);
            Dim = C(dim);
            On = C(on);
            SuccessDim = C(successDim);
            R1 = C(r1);
            R2 = C(r2);
            R4 = C(r4);
            SwatchBrush = new SolidColorBrush(Accent);
        }

        public string Key { get; }
        public string Label { get; }
        public Color Accent { get; }
        public Color Dim { get; }
        public Color On { get; }
        public Color SuccessDim { get; }
        public Color R1 { get; }
        public Color R2 { get; }
        public Color R4 { get; }
        public SolidColorBrush SwatchBrush { get; }

        private static Color C(uint rgb) => Color.FromUInt32(0xFF000000 | rgb);
    }

    public static readonly Preset[] Presets =
    {
        // green is the original palette; the grays run light → dark for a mono look
        new("green",    "Green",    0x4AC265, 0x223322, 0x0C140C, 0x1D3122, 0x3FB950, 0x2EA043, 0x1F6F38),
        new("amber",    "Amber",    0xE8A348, 0x33270F, 0x171006, 0x382B12, 0xD29922, 0xBB8009, 0x8C5A10),
        new("salmon",   "Salmon",   0xF47067, 0x38201D, 0x1A0B09, 0x3B2523, 0xE5534B, 0xC93C37, 0x8E2A24),
        new("gray",     "Gray",     0xA8A8A8, 0x262626, 0x101010, 0x2F2F2F, 0x9A9A9A, 0x8B8B8B, 0x6F6F6F),
        new("silver",   "Silver",   0xC9C9C9, 0x303030, 0x141414, 0x383838, 0xB8B8B8, 0xA8A8A8, 0x8A8A8A),
        new("graphite", "Graphite", 0x7A7A7A, 0x1F1F1F, 0x0E0E0E, 0x292929, 0x6E6E6E, 0x606060, 0x4A4A4A),
        new("charcoal", "Charcoal", 0x505050, 0x1A1A1A, 0x0C0C0C, 0x232323, 0x474747, 0x3E3E3E, 0x303030),
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
        // the SukiUI ramp: Color values to match the originals, Ramp3 = the primary
        resources["ThemeAccentColor"] = p.R1;
        resources["ThemeAccentColor2"] = p.R2;
        resources["ThemeAccentColor3"] = p.Accent;
        resources["ThemeAccentColor4"] = p.R4;
        resources["ThemeAccentBadgeColor"] = Color.FromArgb(0x30, p.R1.R, p.R1.G, p.R1.B);
    }
}
