using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using Avalonia.Media;
using SukiUI;
using SukiUI.Models;
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

        //  the stock Blue primary tints the window shader (titlebar glass) and every
        //  default SukiUI control accent; the green theme keeps color ownership in our brushes
        var green = new SukiColorTheme("Green", Color.Parse("#3FB950"), Color.Parse("#2EA043"));
        var sukiTheme = SukiTheme.GetInstance();
        sukiTheme.AddColorTheme(green);
        sukiTheme.ChangeColorTheme(green);

        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            // fully manual start: the server is only reached via the Start button
            desktop.MainWindow = new MainWindow();
        }
        base.OnFrameworkInitializationCompleted();
    }
}
