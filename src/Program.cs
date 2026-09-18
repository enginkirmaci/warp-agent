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
        return OperatingSystem.IsWindows() ? builder : builder.UseWaylandWithFallback();
    }
}
