using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml;
using Avalonia.Media;
using SukiUI;
using SukiUI.Models;
using WarpAgent.Views;

namespace WarpAgent;

public partial class App : Application
{
    public override void Initialize() => AvaloniaXamlLoader.Load(this);

    public override void OnFrameworkInitializationCompleted()
    {
        //  the stock Blue primary tints the window shader (titlebar glass) and every
        //  default SukiUI control accent; a neutral theme keeps color ownership in our brushes
        var gray = new SukiColorTheme("Gray", Color.Parse("#929292"), Color.Parse("#929292"));
        var sukiTheme = SukiTheme.GetInstance();
        sukiTheme.AddColorTheme(gray);
        sukiTheme.ChangeColorTheme(gray);

        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            // fully manual start: the server is only reached via the Start button
            desktop.MainWindow = new MainWindow();
        }
        base.OnFrameworkInitializationCompleted();
    }
}
