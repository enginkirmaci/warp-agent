using System.Reflection;
using Avalonia.Controls;

namespace WarpAgent.Services;

/// <summary>
/// Avalonia's Wayland backend never sends xdg_toplevel.set_app_id, so the compositor
/// reports class="" and no taskbar/launcher can match our desktop entry. Reach for the
/// xdg_toplevel proxy (the same object SetTitle goes through) and send the app id ourselves.
/// </summary>
public static class WaylandAppId
{
    public static void Apply(TopLevel topLevel, string appId)
    {
        if (!OperatingSystem.IsLinux())
            return;
        try
        {
            var impl = topLevel.PlatformImpl;
            Console.Error.WriteLine($"[appid] impl={impl?.GetType().FullName ?? "null"}");
            if (impl is null)
                return;
            var proxy = FindSurfaceProxy(impl);
            Console.Error.WriteLine($"[appid] proxy={proxy?.GetType().FullName ?? "null"}");
            var m = proxy?.GetType().GetMethod("SetAppId", [typeof(string)]);
            Console.Error.WriteLine($"[appid] method={(m is null ? "null" : "ok")}");
            m?.Invoke(proxy, [appId]);
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"[appid] EX {ex.GetType().Name}: {ex.Message}");
        }
    }

    private static object? FindSurfaceProxy(object impl)
    {
        for (var t = impl.GetType(); t is not null; t = t.BaseType)
        {
            if (t.GetField("_surfaceProxy", BindingFlags.Instance | BindingFlags.NonPublic | BindingFlags.Public) is { } field)
                return field.GetValue(impl);
        }
        return null;
    }
}
