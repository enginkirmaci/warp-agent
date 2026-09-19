using Avalonia;
using Serilog;
using WarpAgent.Services;

namespace WarpAgent;

internal static class Program
{
    [STAThread]
    public static int Main(string[] args)
    {
        AppLogging.Initialize();
        // before the UI builds, so the first frame already honors the desktop text scale
        SystemFontScale.Initialize();
        try
        {
            return BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);
        }
        catch (Exception ex)
        {
            // the window never came up, so there is no UI to show the error on
            Log.Fatal(ex, "startup failure");
            Console.Error.WriteLine(ex);
            return 1;
        }
        finally
        {
            AppLogging.Shutdown();
        }
    }

    public static AppBuilder BuildAvaloniaApp()
    {
        var builder = AppBuilder.Configure<App>()
            .UsePlatformDetect()
            .WithInterFont()
            .LogToTrace();
        if (OperatingSystem.IsWindows())
            return builder;
        // X11 fallback path: keep WM_CLASS matching the desktop entry's StartupWMClass
        if (OperatingSystem.IsLinux())
            builder = builder.With(new X11PlatformOptions { WmClass = "WarpAgent" });
        return builder.UseWaylandWithFallback();
    }
}
