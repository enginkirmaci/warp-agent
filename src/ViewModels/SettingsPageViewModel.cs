using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using Avalonia.Media;
using WarpAgent.Services;

namespace WarpAgent.ViewModels;

/// <summary>Edit buffer for the settings page; Save writes through to the shared AppSettings
/// instance and closes the page via Saved.</summary>
public partial class SettingsPageViewModel : ObservableObject
{
    private readonly MainViewModel _owner;

    [ObservableProperty] private string _openCodeExecutable;
    [ObservableProperty] private string? _defaultModel;
    [ObservableProperty] private string? _defaultVariant;
    [ObservableProperty] private string? _defaultAgent;
    [ObservableProperty] private bool _autoAllow;
    [ObservableProperty] private bool _autoAllowInstant;
    [ObservableProperty] private bool _showToolTips;

    // auto-allow delay buffers: string boxes, parsed on save
    [ObservableProperty] private string _autoAllowDefaultMin;
    [ObservableProperty] private string _autoAllowDefaultMax;
    [ObservableProperty] private string _autoAllowAwayMin;
    [ObservableProperty] private string _autoAllowAwayMax;
    [ObservableProperty] private string _lunchStartHour;
    [ObservableProperty] private string _lunchEndHour;
    [ObservableProperty] private string _offHoursStartHour;
    [ObservableProperty] private string _workdayStartHour;

    // text-size radio group: exactly one of the three is checked
    [ObservableProperty] private bool _isTextCompact;
    [ObservableProperty] private bool _isTextNormal;
    [ObservableProperty] private bool _isTextLarge;

    // accent swatches: radio group over the AccentTheme presets
    public IReadOnlyList<AccentOption> AccentOptions { get; }

    /// <summary>Raised after a successful save; the window closes the page.</summary>
    public event EventHandler? Saved;

    public SettingsPageViewModel(MainViewModel owner)
    {
        _owner = owner;
        var s = owner.Settings;
        _openCodeExecutable = s.OpenCodeExecutable;
        _defaultModel = s.DefaultModel;
        _defaultVariant = s.DefaultVariant;
        _defaultAgent = s.DefaultAgent;
        _autoAllow = s.AutoAllowAlways ?? true;
        _autoAllowInstant = s.AutoAllowInstant;
        _showToolTips = s.ShowToolTips;
        _autoAllowDefaultMin = s.AutoAllowDefaultMinSeconds.ToString();
        _autoAllowDefaultMax = s.AutoAllowDefaultMaxSeconds.ToString();
        _autoAllowAwayMin = s.AutoAllowAwayMinSeconds.ToString();
        _autoAllowAwayMax = s.AutoAllowAwayMaxSeconds.ToString();
        _lunchStartHour = s.LunchStartHour.ToString();
        _lunchEndHour = s.LunchEndHour.ToString();
        _offHoursStartHour = s.OffHoursStartHour.ToString();
        _workdayStartHour = s.WorkdayStartHour.ToString();
        switch (FontScale.Normalize(s.FontScale))
        {
            case FontScale.Compact:
                _isTextCompact = true;
                break;
            case FontScale.Large:
                _isTextLarge = true;
                break;
            default:
                _isTextNormal = true;
                break;
        }
        var accent = AccentTheme.Normalize(s.Accent);
        AccentOptions = AccentTheme.Presets
            .Select(p => new AccentOption(p) { IsSelected = p.Key == accent })
            .ToArray();
    }

    [RelayCommand]
    private void Save()
    {
        var s = _owner.Settings;
        s.OpenCodeExecutable = string.IsNullOrWhiteSpace(OpenCodeExecutable)
            ? AppSettings.DefaultExecutable
            : OpenCodeExecutable.Trim();
        s.DefaultModel = NullIfBlank(DefaultModel);
        s.DefaultVariant = NullIfBlank(DefaultVariant);
        s.DefaultAgent = NullIfBlank(DefaultAgent);
        s.AutoAllowAlways = AutoAllow;
        s.AutoAllowInstant = AutoAllowInstant;
        s.ShowToolTips = ShowToolTips;
        s.AutoAllowDefaultMinSeconds = ParseInt(AutoAllowDefaultMin, s.AutoAllowDefaultMinSeconds, 1);
        s.AutoAllowDefaultMaxSeconds = ParseInt(AutoAllowDefaultMax, s.AutoAllowDefaultMaxSeconds, 15);
        s.AutoAllowAwayMinSeconds = ParseInt(AutoAllowAwayMin, s.AutoAllowAwayMinSeconds, 60);
        s.AutoAllowAwayMaxSeconds = ParseInt(AutoAllowAwayMax, s.AutoAllowAwayMaxSeconds, 600);
        s.LunchStartHour = ParseInt(LunchStartHour, s.LunchStartHour, 12);
        s.LunchEndHour = ParseInt(LunchEndHour, s.LunchEndHour, 14);
        s.OffHoursStartHour = ParseInt(OffHoursStartHour, s.OffHoursStartHour, 18);
        s.WorkdayStartHour = ParseInt(WorkdayStartHour, s.WorkdayStartHour, 8);
        s.FontScale = IsTextCompact ? FontScale.Compact : IsTextLarge ? FontScale.Large : FontScale.Normal;
        s.Accent = AccentOptions.FirstOrDefault(o => o.IsSelected)?.Key;
        s.Save();
        _owner.ApplySavedSettings();
        Saved?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Blank keeps the stored value; garbage falls back to the built-in default.</summary>
    private static int ParseInt(string? text, int current, int fallback) =>
        string.IsNullOrWhiteSpace(text) ? current
        : int.TryParse(text.Trim(), out var value) ? value : fallback;

    private static string? NullIfBlank(string? value) =>
        string.IsNullOrWhiteSpace(value) ? null : value.Trim();
}

/// <summary>One accent swatch on the settings page: a static AccentTheme preset plus radio state.</summary>
public sealed partial class AccentOption : ObservableObject
{
    public AccentOption(AccentTheme.Preset preset) => Preset = preset;

    public AccentTheme.Preset Preset { get; }
    public string Key => Preset.Key;
    public string Label => Preset.Label;
    public IBrush SwatchBrush => Preset.SwatchBrush;

    [ObservableProperty] private bool _isSelected;
}
