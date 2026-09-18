using System.Text.RegularExpressions;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Documents;
using Avalonia.Media;

namespace WarpAgent.Controls;

/// <summary>
/// Renders a unified diff GitHub-style: muted old/new line-number gutter,
/// green/red per-line background tints, gray @@ hunk separators, muted metadata.
/// One SelectableTextBlock so selection still spans lines; Run.Background tints
/// text extent rather than the full row.
/// </summary>
public class DiffTextBlock : StackPanel
{
    private static readonly FontFamily CodeFont = FontFamily.Parse("DejaVu Sans Mono, Consolas, monospace");
    private static readonly Regex HunkRegex = new(@"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", RegexOptions.Compiled);

    private readonly IBrush _addFg = ThemeRes.Brush("AddBrush", "#FF8FC496");
    private readonly IBrush _addBg = new SolidColorBrush(Color.Parse("#268FC496"));
    private readonly IBrush _delFg = ThemeRes.Brush("DelBrush", "#FFE28E85");
    private readonly IBrush _delBg = new SolidColorBrush(Color.Parse("#26E28E85"));
    private readonly IBrush _hunkFg = ThemeRes.Brush("AccentBrush", "#FFBBBBBB");
    private readonly IBrush _hunkBg = new SolidColorBrush(Color.Parse("#1FBBBBBB"));
    private readonly IBrush _metaFg = ThemeRes.Brush("TextMutedBrush", "#FF929292");
    private readonly IBrush _gutterFg = new SolidColorBrush(Color.Parse("#55929292"));
    private readonly IBrush _textFg = ThemeRes.Brush("CodeFgBrush", "#FFDADADA");

    public static readonly StyledProperty<string?> TextProperty =
        AvaloniaProperty.Register<DiffTextBlock, string?>(nameof(Text));

    public string? Text
    {
        get => GetValue(TextProperty);
        set => SetValue(TextProperty, value);
    }

    public DiffTextBlock()
    {
        Spacing = 1;
    }

    // per-instance override, not TextProperty.Changed.AddClassHandler in the constructor:
    // that adds one never-disposed static subscription per instance (leak + O(N) dispatch)
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs e)
    {
        base.OnPropertyChanged(e);
        if (e.Property == TextProperty)
            Rebuild();
    }

    protected override void OnAttachedToVisualTree(VisualTreeAttachmentEventArgs e)
    {
        base.OnAttachedToVisualTree(e);
        Rebuild();
    }

    private void Rebuild()
    {
        Children.Clear();
        if (Text is not { Length: > 0 } text)
            return;
        var tb = new SelectableTextBlock
        {
            FontFamily = CodeFont,
            FontSize = 11.5,
            TextWrapping = TextWrapping.Wrap,
        };
        tb.Inlines ??= new InlineCollection();

        var old = 0;
        var cur = 0;
        var first = true;
        foreach (var line in text.Replace("\r", "").Split('\n'))
        {
            if (!first)
                tb.Inlines.Add(new LineBreak());
            first = false;

            var hunk = HunkRegex.Match(line);
            if (hunk.Success)
            {
                old = int.Parse(hunk.Groups[1].Value);
                cur = int.Parse(hunk.Groups[2].Value);
                tb.Inlines.Add(new Run(line) { Foreground = _hunkFg, Background = _hunkBg });
                continue;
            }

            if (IsMeta(line))
            {
                tb.Inlines.Add(new Run(line.Length == 0 ? " " : line) { Foreground = _metaFg });
                continue;
            }

            if (line.StartsWith('+'))
            {
                tb.Inlines.Add(Gutter($"     {cur,4}  "));
                cur++;
                tb.Inlines.Add(new Run(line.Length == 0 ? " " : line) { Foreground = _addFg, Background = _addBg });
            }
            else if (line.StartsWith('-'))
            {
                tb.Inlines.Add(Gutter($"{old,4}      "));
                old++;
                tb.Inlines.Add(new Run(line.Length == 0 ? " " : line) { Foreground = _delFg, Background = _delBg });
            }
            else
            {
                tb.Inlines.Add(Gutter($"{old,4} {cur,4}  "));
                old++;
                cur++;
                if (line.Length > 0)
                    tb.Inlines.Add(new Run(line) { Foreground = _textFg });
            }
        }
        Children.Add(tb);
    }

    private Run Gutter(string text) => new(text) { Foreground = _gutterFg };

    private static bool IsMeta(string line) =>
        line.StartsWith("diff --git") || line.StartsWith("index ") || line.StartsWith("--- ") ||
        line.StartsWith("+++ ") || line.StartsWith("new file") || line.StartsWith("deleted file") ||
        line.StartsWith("old mode") || line.StartsWith("new mode") || line.StartsWith("similarity") ||
        line.StartsWith("rename ") || line.StartsWith("Binary files") || line.StartsWith("GIT binary patch") ||
        line.StartsWith("\\");
}
