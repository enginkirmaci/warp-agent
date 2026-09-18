using Avalonia;
using Avalonia.Media;

namespace WarpAgent.Controls;

// App.xaml holds the palette; controls resolve named brushes at build time and
// fall back to hex so they still render where the resources aren't loaded.
// Named ThemeRes, not Theme — Theme is StyledElement's own property.
internal static class ThemeRes
{
    internal static IBrush Brush(string key, string fallback) =>
        Application.Current?.TryGetResource(key, null, out var value) == true && value is IBrush brush
            ? brush
            : new SolidColorBrush(Color.Parse(fallback));

    internal static StreamGeometry Geometry(string key, string fallback) =>
        Application.Current?.TryGetResource(key, null, out var value) == true && value is StreamGeometry g
            ? g
            : StreamGeometry.Parse(fallback);
}
