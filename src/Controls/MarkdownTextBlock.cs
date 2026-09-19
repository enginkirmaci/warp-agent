using System.Diagnostics;
using System.Text;
using System.Text.RegularExpressions;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Documents;
using Avalonia.Input;
using Avalonia.Input.Platform;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Threading;
using WarpAgent.Services;

namespace WarpAgent.Controls;

/// <summary>
/// Lightweight markdown renderer for chat answers: headings, bold/italic, inline
/// code, fenced code (basic syntax highlighting + copy), lists, links, tables,
/// blockquotes, rules. Rebuilds its children on every Text change so streaming
/// updates just work.
/// </summary>
public class MarkdownTextBlock : StackPanel
{
    private static readonly FontFamily CodeFont = FontFamily.Parse("DejaVu Sans Mono, Consolas, monospace");
    private static readonly Regex LinkRegex = new(@"\[([^\]]+)\]\(([^)\s]+)\)", RegexOptions.Compiled);
    private static readonly Regex OrderedRegex = new(@"^\d+[.)]\s+(.*)$", RegexOptions.Compiled);
    private static readonly Regex TableSepRegex = new(@"^\s*\|?(\s*:?-+:?\s*\|)+\s*:?-+:?\s*\|?\s*$", RegexOptions.Compiled);

    private static readonly StreamGeometry CopyGeometry =
        ThemeRes.Geometry("IconCopy", "F0 M8 2 H20 A2 2 0 0 1 22 4 V16 H20 V4 H8 Z M4 6 H16 A2 2 0 0 1 18 8 V20 A2 2 0 0 1 16 22 H4 A2 2 0 0 1 2 20 V8 A2 2 0 0 1 4 6 Z");

    private readonly IBrush _inlineCodeBg = ThemeRes.Brush("ChipBrush", "#FF343434");
    private readonly IBrush _codeFg = ThemeRes.Brush("CodeFgBrush", "#FFE4E4E4");
    private readonly IBrush _blockBg = ThemeRes.Brush("CodeBlockBrush", "#FF171717");
    private readonly IBrush _headerBg = ThemeRes.Brush("ChipBrush", "#FF343434");
    private readonly IBrush _linkFg = ThemeRes.Brush("AccentBrush", "#FF4AC265");
    private readonly IBrush _kw = ThemeRes.Brush("SynKeywordBrush", "#FFA9A9A9");
    private readonly IBrush _str = ThemeRes.Brush("SynStringBrush", "#FF93B585");
    private readonly IBrush _com = ThemeRes.Brush("SynCommentBrush", "#FF6F6F6F");
    private readonly IBrush _num = ThemeRes.Brush("SynNumberBrush", "#FFC9986E");
    private readonly IBrush _muted = ThemeRes.Brush("TextMutedBrush", "#FF9A9A9A");
    private readonly IBrush _hairline = ThemeRes.Brush("HairlineBrush", "#FF3E3E3E");

    public static readonly StyledProperty<string> TextProperty =
        AvaloniaProperty.Register<MarkdownTextBlock, string>(nameof(Text), "");

    public string Text
    {
        get => GetValue(TextProperty);
        set => SetValue(TextProperty, value);
    }

    public MarkdownTextBlock()
    {
        Spacing = 6;
    }

    // per-instance override, not TextProperty.Changed.AddClassHandler in the constructor:
    // that adds one never-disposed static subscription per instance (leak + O(N) dispatch)
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs e)
    {
        base.OnPropertyChanged(e);
        if (e.Property == TextProperty)
            QueueRebuild();
    }

    protected override void OnAttachedToVisualTree(VisualTreeAttachmentEventArgs e)
    {
        base.OnAttachedToVisualTree(e);
        // attached-only subscription: detached instances must not linger on the static event
        FontScale.Changed += OnFontScaleChanged;
        if (_builtFor is null)
            FlushRebuild(); // first show builds immediately so the tile doesn't flash empty
        else if (_builtFor != Text)
            QueueRebuild(); // stale; the streaming throttle covers it
    }

    protected override void OnDetachedFromVisualTree(VisualTreeAttachmentEventArgs e)
    {
        base.OnDetachedFromVisualTree(e);
        FontScale.Changed -= OnFontScaleChanged;
    }

    private void OnFontScaleChanged(object? sender, EventArgs e) => QueueRebuild();

    // Streaming rewrites Text many times per second with the full text-so-far; a full
    // parse + visual rebuild per delta is O(n²) on long answers. Coalescing to one
    // rebuild per ~32 ms keeps up visually while cutting the work ~10-30x.
    private DispatcherTimer? _rebuildTimer;
    private bool _rebuildPending;

    private void QueueRebuild()
    {
        if (_rebuildPending)
            return;
        _rebuildPending = true;
        _rebuildTimer ??= new DispatcherTimer(TimeSpan.FromMilliseconds(32), DispatcherPriority.Background,
            (_, _) => FlushRebuild());
        _rebuildTimer.Start();
    }

    private void FlushRebuild()
    {
        _rebuildTimer?.Stop();
        _rebuildPending = false;
        Rebuild();
    }

    private void Rebuild()
    {
        _builtFor = Text;
        Children.Clear();
        foreach (var block in Parse(Text))
            Children.Add(Build(block));
    }

    private string? _builtFor;

    private Control Build(Block block) => block switch
    {
        Heading h => HeadingBlock(h),
        Code code => CodeBlock(code),
        List list => ListBlock(list),
        Quote quote => QuoteBlock(quote),
        Table table => TableBlock(table),
        Rule => new Border { Height = 1, Background = _hairline, Opacity = 0.7, Margin = new Thickness(0, 3) },
        _ => Paragraph(((ParaBlock)block).Text),
    };

    private static SelectableTextBlock HeadingBlock(Heading h)
    {
        var tb = NewTextBlock();
        tb.FontWeight = FontWeight.Bold;
        tb.FontSize = h.Level switch
        {
            1 => ThemeRes.Double("FontH1", 18),
            2 => ThemeRes.Double("FontH2", 16),
            _ => ThemeRes.Double("FontH3", 14.5),
        };
        tb.Margin = new Thickness(0, 4, 0, 0);
        AddInline(tb, h.Text, bold: true);
        return tb;
    }

    private Control CodeBlock(Code code)
    {
        var text = new SelectableTextBlock
        {
            FontFamily = CodeFont,
            FontSize = ThemeRes.Double("FontBody", 12.5),
            TextWrapping = TextWrapping.Wrap,
        };
        text.Inlines ??= new InlineCollection();
        foreach (var inline in Highlight(code.Text, code.Lang))
            text.Inlines.Add(inline);

        var copy = new Button
        {
            Content = new PathIcon { Width = 13, Height = 13, Data = CopyGeometry },
            Padding = new Thickness(3),
            Background = Brushes.Transparent,
            HorizontalAlignment = HorizontalAlignment.Right,
            VerticalAlignment = VerticalAlignment.Center,
        };
        copy.Classes.Add("code-copy");
        ToolTip.SetTip(copy, "Copy code");
        copy.Click += async (_, _) =>
        {
            try
            {
                if (TopLevel.GetTopLevel(this)?.Clipboard is { } clipboard)
                    await clipboard.SetTextAsync(code.Text);
            }
            catch (Exception ex)
            {
                Debug.WriteLine($"[code-copy] {ex.Message}");
            }
        };

        var header = new DockPanel { LastChildFill = true };
        DockPanel.SetDock(copy, Dock.Right);
        header.Children.Add(copy);
        header.Children.Add(new TextBlock
        {
            Text = code.Lang.Length > 0 ? code.Lang : "code",
            FontSize = ThemeRes.Double("FontMicro", 10.5),
            FontFamily = CodeFont,
            Foreground = _muted,
            VerticalAlignment = VerticalAlignment.Center,
            Margin = new Thickness(0, 0, 6, 0),
        });

        var card = new Border
        {
            Background = _blockBg,
            CornerRadius = new CornerRadius(8),
            ClipToBounds = true,
            Child = new StackPanel
            {
                Children =
                {
                    new Border { Background = _headerBg, Padding = new Thickness(9, 4), Child = header },
                    new Border { Padding = new Thickness(10, 8), Child = text },
                },
            },
        };
        card.Classes.Add("code-block");
        return card;
    }

    private static Control ListBlock(List list)
    {
        var panel = new StackPanel { Spacing = 3 };
        for (var i = 0; i < list.Items.Count; i++)
        {
            var tb = NewTextBlock();
            tb.Margin = new Thickness(14, 0, 0, 0);
            tb.Inlines ??= new InlineCollection();
            tb.Inlines.Add(new Run(list.Ordered ? $"{i + 1}.  " : "•  "));
            foreach (var inline in Inlines(list.Items[i]))
                tb.Inlines.Add(inline);
            panel.Children.Add(tb);
        }
        return panel;
    }

    private Control QuoteBlock(Quote quote)
    {
        var tb = NewTextBlock();
        tb.Opacity = 0.75;
        AddInline(tb, quote.Text, bold: false);
        return new Border
        {
            BorderBrush = _linkFg,
            BorderThickness = new Thickness(2.5, 0, 0, 0),
            Padding = new Thickness(9, 2),
            Margin = new Thickness(0, 1),
            Child = tb,
        };
    }

    private Control TableBlock(Table table)
    {
        var cols = table.Rows.Max(r => r.Length);
        var grid = new Grid { ColumnSpacing = 16, RowSpacing = 4 };
        for (var c = 0; c < cols; c++)
            grid.ColumnDefinitions.Add(new ColumnDefinition(1, GridUnitType.Star));
        grid.RowDefinitions.Add(new RowDefinition(0, GridUnitType.Auto));

        for (var c = 0; c < table.Rows[0].Length; c++)
        {
            var tb = NewTextBlock();
            tb.FontWeight = FontWeight.Bold;
            AddInline(tb, table.Rows[0][c], bold: true);
            Grid.SetRow(tb, 0);
            Grid.SetColumn(tb, c);
            grid.Children.Add(tb);
        }

        grid.RowDefinitions.Add(new RowDefinition(0, GridUnitType.Auto));
        var hairline = new Border { Height = 1, Background = _hairline, Margin = new Thickness(0, 2) };
        Grid.SetRow(hairline, 1);
        Grid.SetColumnSpan(hairline, cols);
        grid.Children.Add(hairline);

        for (var r = 1; r < table.Rows.Count; r++)
        {
            grid.RowDefinitions.Add(new RowDefinition(0, GridUnitType.Auto));
            for (var c = 0; c < table.Rows[r].Length; c++)
            {
                var tb = NewTextBlock();
                AddInline(tb, table.Rows[r][c], bold: false);
                Grid.SetRow(tb, r + 1);
                Grid.SetColumn(tb, c);
                grid.Children.Add(tb);
            }
        }
        return grid;
    }

    private static SelectableTextBlock Paragraph(string text)
    {
        var tb = NewTextBlock();
        AddInline(tb, text, bold: false);
        return tb;
    }

    private static SelectableTextBlock NewTextBlock() =>
        new() { TextWrapping = TextWrapping.Wrap, HorizontalAlignment = HorizontalAlignment.Stretch };

    private static void AddInline(SelectableTextBlock tb, string text, bool bold)
    {
        tb.Inlines ??= new InlineCollection();
        foreach (var inline in Inlines(text, bold))
            tb.Inlines.Add(inline);
    }

    private static IEnumerable<Inline> Inlines(string text, bool bold = false, int depth = 0)
    {
        var plain = new StringBuilder();
        var i = 0;
        IEnumerable<Inline> Flush()
        {
            if (plain.Length > 0)
            {
                yield return MakeRun(plain.ToString(), bold, mono: false, link: false);
                plain.Clear();
            }
        }

        while (i < text.Length)
        {
            var ch = text[i];
            if (ch == '`')
            {
                var end = text.IndexOf('`', i + 1);
                if (end > i)
                {
                    foreach (var inline in Flush()) yield return inline;
                    yield return MakeRun(text[(i + 1)..end], bold: false, mono: true, link: false);
                    i = end + 1;
                    continue;
                }
            }
            else if (ch == '*' && i + 1 < text.Length && text[i + 1] == '*' && depth < 2)
            {
                var end = text.IndexOf("**", i + 2, StringComparison.Ordinal);
                if (end > 0)
                {
                    foreach (var inline in Flush()) yield return inline;
                    foreach (var inline in Inlines(text[(i + 2)..end], bold: true, depth + 1))
                        yield return inline;
                    i = end + 2;
                    continue;
                }
            }
            else if (ch == '*' && depth < 2 && (i == 0 || text[i - 1] != '`') && i + 1 < text.Length && !char.IsWhiteSpace(text[i + 1]))
            {
                var end = text.IndexOf('*', i + 1);
                if (end > i)
                {
                    foreach (var inline in Flush()) yield return inline;
                    foreach (var inline in Inlines(text[(i + 1)..end], bold, depth + 1))
                        yield return inline;
                    i = end + 1;
                    continue;
                }
            }
            else if (ch == '[')
            {
                var match = LinkRegex.Match(text, i);
                if (match.Success && match.Index == i)
                {
                    foreach (var inline in Flush()) yield return inline;
                    yield return MakeLink(match.Groups[2].Value, match.Groups[1].Value, bold);
                    i = match.Index + match.Length;
                    continue;
                }
            }
            plain.Append(ch);
            i++;
        }

        foreach (var inline in Flush()) yield return inline;
    }

    // per-run values: resolved once, never per inline (resource walks + parses add up on long answers)
    private static readonly IBrush InlineChipBg = ThemeRes.Brush("ChipBrush", "#FF343434");
    private static readonly IBrush InlineCodeFg = ThemeRes.Brush("CodeFgBrush", "#FFE4E4E4");
    private static readonly IBrush AccentFg = ThemeRes.Brush("AccentBrush", "#FF4AC265");
    private static readonly TextDecorationCollection Underline = TextDecorationCollection.Parse("Underline");

    private static Inline MakeLink(string url, string label, bool bold)
    {
        var tb = new TextBlock
        {
            Text = label,
            Cursor = new Cursor(StandardCursorType.Hand),
            Foreground = AccentFg,
            TextDecorations = Underline,
            FontWeight = bold ? FontWeight.Bold : FontWeight.Normal,
        };
        tb.PointerReleased += (_, e) =>
        {
            e.Handled = true;
            OpenUrl(url);
        };
        return new InlineUIContainer { Child = tb };
    }

    private static void OpenUrl(string url)
    {
        if (!url.StartsWith("http://", StringComparison.Ordinal) && !url.StartsWith("https://", StringComparison.Ordinal))
            return;
        try
        {
            using var _ = Process.Start(new ProcessStartInfo(url) { UseShellExecute = true });
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"[link] {ex.Message}");
        }
    }

    private static Run MakeRun(string text, bool bold, bool mono, bool link)
    {
        var run = new Run(text);
        if (mono)
        {
            run.FontFamily = CodeFont;
            run.FontSize = ThemeRes.Double("FontBody", 12.5);
            run.Background = InlineChipBg;
            run.Foreground = InlineCodeFg;
        }
        else if (link)
        {
            run.Foreground = AccentFg;
            run.TextDecorations = Underline;
        }
        if (bold)
            run.FontWeight = FontWeight.Bold;
        return run;
    }

    // ---- syntax highlighting ----

    private enum HighlightMode
    {
        Generic,
        Hash, // '#' line comments: python, shell, yaml…
        Sql,  // '--' line comments
        Markup, // <tags>
    }

    private static readonly HashSet<string> CLikeWords = new(StringComparer.Ordinal)
    {
        "abstract","alignas","and","as","asm","assert","async","await","base","bool","break","byte","case","catch","chan","char","checked","class","const","constexpr","continue","crate","decimal","default","defer","delegate","delete","do","double","dyn","dynamic","else","enum","event","explicit","export","extends","extern","false","final","finally","fixed","float","fn","for","foreach","friend","from","func","function","get","go","goto","if","impl","implements","import","in","inline","instanceof","int","interface","internal","is","let","lock","long","loop","match","mod","move","mut","namespace","new","nil","noexcept","not","null","nullptr","object","operator","or","out","override","package","params","private","protected","pub","public","readonly","ref","register","return","sbyte","sealed","select","self","Self","set","short","signed","sizeof","static","stackalloc","string","struct","super","switch","synchronized","template","this","throw","throws","trait","true","try","type","typedef","typeof","uint","ulong","unchecked","union","unsafe","unsigned","use","ushort","using","var","virtual","void","volatile","when","where","while","with","yield",
    };

    private static readonly HashSet<string> PythonWords = new(StringComparer.Ordinal)
    {
        "and","as","assert","async","await","class","def","del","elif","else","except","False","finally","for","from","global","if","import","in","is","lambda","None","nonlocal","not","or","pass","raise","return","self","True","try","while","with","yield",
    };

    private static readonly HashSet<string> ShellWords = new(StringComparer.Ordinal)
    {
        "alias","break","case","cd","continue","do","done","echo","elif","else","esac","eval","exec","exit","export","fi","for","function","if","in","local","read","readonly","return","select","set","shift","source","then","trap","unset","until","while",
    };

    private static readonly HashSet<string> SqlWords = new(StringComparer.OrdinalIgnoreCase)
    {
        "add","all","alter","and","as","asc","begin","between","by","case","column","commit","constraint","create","cross","default","delete","desc","distinct","drop","else","end","exists","foreign","from","full","group","having","in","index","inner","insert","into","is","join","key","left","like","limit","not","null","offset","on","or","order","outer","primary","references","rollback","select","set","table","then","union","unique","update","values","view","when","where",
    };

    private static readonly HashSet<string> JsonWords = new(StringComparer.Ordinal) { "true", "false", "null" };

    private static (HashSet<string>? Words, HighlightMode Mode) LangInfo(string lang) => lang switch
    {
        "sql" => (SqlWords, HighlightMode.Sql),
        "json" => (JsonWords, HighlightMode.Generic),
        "xml" or "html" or "xaml" or "svg" or "vue" or "jsx" or "tsx" => (null, HighlightMode.Markup),
        "py" or "python" or "ruby" or "rb" or "yaml" or "yml" or "toml" or "r" or "perl" => (PythonWords, HighlightMode.Hash),
        "sh" or "bash" or "shell" or "zsh" or "fish" or "console" or "ps1" or "powershell" or "dockerfile" or "makefile" => (ShellWords, HighlightMode.Hash),
        _ => (CLikeWords, HighlightMode.Generic),
    };

    private IEnumerable<Inline> Highlight(string code, string lang)
    {
        var inlines = new List<Inline>();
        var (words, mode) = LangInfo(lang);
        var inBlockComment = false;
        var lines = code.Replace("\r", "").Split('\n');
        for (var li = 0; li < lines.Length; li++)
        {
            if (li > 0)
                inlines.Add(new LineBreak());
            HighlightLine(lines[li], words, mode, ref inBlockComment, inlines);
        }
        return inlines;
    }

    private void HighlightLine(string line, HashSet<string>? words, HighlightMode mode, ref bool inBlockComment, List<Inline> into)
    {
        var plain = new StringBuilder();
        var i = 0;
        void Flush()
        {
            if (plain.Length > 0)
            {
                into.Add(Plain(plain.ToString()));
                plain.Clear();
            }
        }

        while (i < line.Length)
        {
            if (inBlockComment)
            {
                var end = line.IndexOf("*/", i, StringComparison.Ordinal);
                if (end < 0)
                {
                    into.Add(Comment(line[i..]));
                    return;
                }
                into.Add(Comment(line[i..(end + 2)]));
                i = end + 2;
                inBlockComment = false;
                continue;
            }

            var ch = line[i];

            if (ch == '/' && i + 1 < line.Length && line[i + 1] == '*' && mode != HighlightMode.Markup)
            {
                Flush();
                var end = line.IndexOf("*/", i + 2, StringComparison.Ordinal);
                if (end < 0)
                {
                    inBlockComment = true;
                    into.Add(Comment(line[i..]));
                    return;
                }
                into.Add(Comment(line[i..(end + 2)]));
                i = end + 2;
                continue;
            }

            if (ch == '/' && i + 1 < line.Length && line[i + 1] == '/' && mode is HighlightMode.Generic)
            {
                Flush();
                into.Add(Comment(line[i..]));
                return;
            }

            if (ch == '#' && mode == HighlightMode.Hash && (i == 0 || OnlySpaceBefore(line, i)))
            {
                Flush();
                into.Add(Comment(line[i..]));
                return;
            }

            if (mode == HighlightMode.Sql && ch == '-' && i + 1 < line.Length && line[i + 1] == '-' && (i == 0 || OnlySpaceBefore(line, i)))
            {
                Flush();
                into.Add(Comment(line[i..]));
                return;
            }

            if (ch is '"' or '\'' or '`')
            {
                Flush();
                var end = FindStringEnd(line, i + 1, ch);
                into.Add(Str(line[i..end]));
                i = end;
                continue;
            }

            if (mode == HighlightMode.Markup && ch == '<')
            {
                Flush();
                var j = i + 1;
                if (j < line.Length && line[j] == '/')
                    j++;
                while (j < line.Length && (char.IsLetterOrDigit(line[j]) || line[j] is ':' or '-' or '_' or '!'))
                    j++;
                into.Add(Keyword(line[i..j]));
                i = j;
                continue;
            }

            if (char.IsDigit(ch) && (i == 0 || !IsIdentChar(line[i - 1])))
            {
                var j = i + 1;
                while (j < line.Length && (char.IsLetterOrDigit(line[j]) || line[j] == '.'))
                    j++;
                Flush();
                into.Add(Num(line[i..j]));
                i = j;
                continue;
            }

            if (char.IsLetter(ch) || ch == '_')
            {
                var j = i + 1;
                while (j < line.Length && IsIdentChar(line[j]))
                    j++;
                var word = line[i..j];
                Flush();
                into.Add(words is not null && words.Contains(word) ? Keyword(word) : Plain(word));
                i = j;
                continue;
            }

            plain.Append(ch);
            i++;
        }
        Flush();
    }

    private static bool OnlySpaceBefore(string line, int i) => line[..i].AsSpan().Trim().Length == 0;

    private static int FindStringEnd(string line, int start, char quote)
    {
        for (var i = start; i < line.Length; i++)
        {
            if (line[i] == '\\' && quote != '`')
                i++;
            else if (line[i] == quote)
                return i + 1;
        }
        return line.Length;
    }

    private static bool IsIdentChar(char c) => char.IsLetterOrDigit(c) || c == '_';

    private Run Plain(string text) => new(text) { Foreground = _codeFg };
    private Run Keyword(string text) => new(text) { Foreground = _kw };
    private Run Str(string text) => new(text) { Foreground = _str };
    private Run Comment(string text) => new(text) { Foreground = _com, FontStyle = FontStyle.Italic };
    private Run Num(string text) => new(text) { Foreground = _num };

    // ---- block parsing ----

    private abstract record Block;

    private sealed record ParaBlock(string Text) : Block;

    private sealed record Heading(int Level, string Text) : Block;

    private sealed record Code(string Text, string Lang) : Block;

    private sealed record List(bool Ordered, IReadOnlyList<string> Items) : Block;

    private sealed record Quote(string Text) : Block;

    private sealed record Table(IReadOnlyList<string[]> Rows) : Block;

    private sealed record Rule : Block;

    private static List<Block> Parse(string? text)
    {
        var blocks = new List<Block>();
        if (string.IsNullOrEmpty(text))
            return blocks;

        var lines = text.Split('\n');
        var para = new List<string>();
        var listItems = new List<string>();
        var listOrdered = false;
        var inList = false;
        var inQuote = false;
        var quote = new StringBuilder();
        var fenceChar = '\0';
        var fenceLang = "";
        var code = new StringBuilder();

        void FlushPara()
        {
            if (para.Count > 0)
            {
                blocks.Add(new ParaBlock(string.Join('\n', para)));
                para.Clear();
            }
        }

        void FlushList()
        {
            if (inList)
            {
                blocks.Add(new List(listOrdered, listItems.ToArray()));
                listItems.Clear();
                inList = false;
            }
        }

        void FlushQuote()
        {
            if (inQuote)
            {
                blocks.Add(new Quote(quote.ToString().TrimEnd()));
                quote.Clear();
                inQuote = false;
            }
        }

        static bool IsRule(string trimmed) =>
            trimmed.Length >= 3 && trimmed[0] is '-' or '*' or '_' && trimmed.All(c => c == trimmed[0]);

        for (var i = 0; i < lines.Length;)
        {
            var line = lines[i].TrimEnd('\r');

            if (fenceChar != '\0')
            {
                if (line.TrimStart().StartsWith(fenceChar))
                {
                    blocks.Add(new Code(code.ToString(), fenceLang));
                    code.Clear();
                    fenceChar = '\0';
                    fenceLang = "";
                }
                else
                {
                    code.Append(line).Append('\n');
                }
                i++;
                continue;
            }

            var trimmed = line.Trim();
            if (trimmed.StartsWith("```") || trimmed.StartsWith("~~~"))
            {
                FlushPara();
                FlushList();
                FlushQuote();
                fenceChar = trimmed[0];
                fenceLang = trimmed[3..].Trim().Split(' ', '\t')[0].ToLowerInvariant();
                i++;
                continue;
            }

            if (trimmed.Length == 0)
            {
                FlushPara();
                FlushList();
                FlushQuote();
                i++;
                continue;
            }

            if (IsRule(trimmed))
            {
                FlushPara();
                FlushList();
                FlushQuote();
                blocks.Add(new Rule());
                i++;
                continue;
            }

            if (trimmed.StartsWith('>'))
            {
                FlushPara();
                FlushList();
                inQuote = true;
                quote.AppendLine(trimmed.Length > 1 && trimmed[1] == ' ' ? trimmed[2..] : trimmed[1..]);
                i++;
                continue;
            }

            if (trimmed.StartsWith('|') && i + 1 < lines.Length && TableSepRegex.IsMatch(lines[i + 1].TrimEnd('\r')))
            {
                FlushPara();
                FlushList();
                FlushQuote();
                var rows = new List<string[]> { Cells(trimmed) };
                i += 2; // header + separator
                while (i < lines.Length)
                {
                    var row = lines[i].TrimEnd('\r').Trim();
                    if (!row.StartsWith('|'))
                        break;
                    rows.Add(Cells(row));
                    i++;
                }
                blocks.Add(new Table(rows));
                continue;
            }

            var level = 0;
            while (level < trimmed.Length && trimmed[level] == '#')
                level++;
            if (level is >= 1 and <= 6 && level < trimmed.Length && trimmed[level] == ' ')
            {
                FlushPara();
                FlushList();
                FlushQuote();
                blocks.Add(new Heading(Math.Min(level, 3), trimmed[(level + 1)..].Trim()));
                i++;
                continue;
            }

            var orderedMatch = OrderedRegex.Match(trimmed);
            if (trimmed.StartsWith("- ") || trimmed.StartsWith("* ") || trimmed.StartsWith("+ ") || orderedMatch.Success)
            {
                FlushPara();
                FlushQuote();
                var ordered = orderedMatch.Success;
                if (inList && ordered != listOrdered)
                    FlushList();
                listOrdered = ordered;
                inList = true;
                listItems.Add(ordered ? orderedMatch.Groups[1].Value : trimmed[2..].Trim());
                i++;
                continue;
            }

            FlushList();
            FlushQuote();
            para.Add(trimmed);
            i++;
        }

        if (fenceChar != '\0')
            blocks.Add(new Code(code.ToString(), fenceLang));
        FlushPara();
        FlushList();
        FlushQuote();
        return blocks;
    }

    private static string[] Cells(string row)
    {
        var inner = row.Trim();
        if (inner.StartsWith('|'))
            inner = inner[1..];
        if (inner.EndsWith('|'))
            inner = inner[..^1];
        return inner.Split('|').Select(c => c.Trim()).ToArray();
    }
}
