using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
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

    // text-size radio group: exactly one of the three is checked
    [ObservableProperty] private bool _isTextCompact;
    [ObservableProperty] private bool _isTextNormal;
    [ObservableProperty] private bool _isTextLarge;

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
        s.FontScale = IsTextCompact ? FontScale.Compact : IsTextLarge ? FontScale.Large : FontScale.Normal;
        s.Save();
        _owner.ApplySavedSettings();
        Saved?.Invoke(this, EventArgs.Empty);
    }

    private static string? NullIfBlank(string? value) =>
        string.IsNullOrWhiteSpace(value) ? null : value.Trim();
}
