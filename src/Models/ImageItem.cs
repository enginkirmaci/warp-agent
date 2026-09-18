namespace WarpAgent.Models;

public sealed record FileAttachment(string FileName, string Mime, byte[] Data)
{
    public string DataUrl => "data:" + Mime + ";base64," + Convert.ToBase64String(Data);
    public bool IsImage => Mime.StartsWith("image/", StringComparison.Ordinal);
}

public sealed class ImageItem(string fileName, byte[] png, DateTime? createdAt = null) : ChatItem
{
    public string FileName { get; } = fileName;
    public byte[] Png { get; } = png;
    public DateTime? CreatedAt { get; } = createdAt;
    public string TimeLabel => TimeLabels.Message(CreatedAt);

    private Avalonia.Media.Imaging.Bitmap? _preview;

    /// <summary>Decoded lazily on first template bind, which always happens on the UI thread.</summary>
    public Avalonia.Media.Imaging.Bitmap Preview => _preview ??= Decode(Png);

    private static Avalonia.Media.Imaging.Bitmap Decode(byte[] png)
    {
        using var ms = new MemoryStream(png);
        return Avalonia.Media.Imaging.Bitmap.DecodeToWidth(ms, 320);
    }

    public static ImageItem? FromDataUrl(string? url, string fileName, DateTime? createdAt = null)
    {
        if (url is null || !url.StartsWith("data:image", StringComparison.Ordinal))
            return null;
        var comma = url.IndexOf(',');
        if (comma < 0)
            return null;
        try
        {
            return new ImageItem(fileName, Convert.FromBase64String(url[(comma + 1)..]), createdAt);
        }
        catch (FormatException)
        {
            return null;
        }
    }
}
