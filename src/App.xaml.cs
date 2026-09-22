using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using WarpAgent.Services;
using WarpAgent.Views;

namespace WarpAgent;

public partial class App : Application
{
    public override void Initialize() => AvaloniaXamlLoader.Load(this);

    public override void OnFrameworkInitializationCompleted()
    {
        // first thing: no managed exception may crash the app — a crash stops every session
        CrashGuard.Install();

        // AccentTheme.Apply (MainViewModel ctor) installs the Suki color theme from the
        // saved accent preset; the stock Blue default in App.xaml only lives until then

        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            // fully manual start: the server is only reached via the Start button
            desktop.MainWindow = new MainWindow();
        }
        base.OnFrameworkInitializationCompleted();
    }
}
