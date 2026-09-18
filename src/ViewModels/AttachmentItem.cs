using System.IO;
using Avalonia.Media.Imaging;
using CommunityToolkit.Mvvm.ComponentModel;

namespace WarpAgent.ViewModels;

public sealed class AttachmentItem(string fileName, string mime, byte[] data) : ObservableObject
{
    public string FileName { get; } = fileName;

    public string Mime { get; } = mime;

    public byte[] Data { get; } = data;

    public bool IsImage => Mime.StartsWith("image/", StringComparison.Ordinal);

    /// <summary>Null for non-image attachments, which render as a file chip instead.</summary>
    public Bitmap? Preview { get; } = mime.StartsWith("image/", StringComparison.Ordinal) ? Decode(data) : null;

    private static Bitmap? Decode(byte[] data)
    {
        try
        {
            using var ms = new MemoryStream(data);
            return Bitmap.DecodeToWidth(ms, 96);
        }
        catch
        {
            return null;
        }
    }
}
