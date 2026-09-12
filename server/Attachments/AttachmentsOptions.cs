using System.Globalization;

namespace Vorcall.Server.Attachments;

// Where uploaded images live and how many bytes of them the server will keep. The directory has
// a usable default; a quota that is present but unusable fails the boot like the other Vorcall
// options, because silently falling back to 2 GiB on a typo is how a disk fills up.
public sealed record AttachmentsOptions
{
    public const string DefaultDir = "/attachments";

    public const long DefaultMaxBytes = 2L << 30;

    // Per file, from PROTOCOL.md § Attachments: above this the upload is refused with 413.
    public const int MaxFileBytes = 8 << 20;

    public const int MaxPerMessage = 4;

    // Enough for the longest magic number the four accepted types use (RIFF....WEBP).
    public const int MagicLength = 12;

    // An upload nothing linked to a message within this is swept, file and row together; the same
    // age makes an unreferenced image sweepable.
    public static readonly TimeSpan UnlinkedTtl = TimeSpan.FromHours(1);

    private static ReadOnlySpan<byte> PngMagic => [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

    private static ReadOnlySpan<byte> JpegMagic => [0xFF, 0xD8, 0xFF];

    private AttachmentsOptions(string dir, long maxBytes)
    {
        Dir = dir;
        MaxBytes = maxBytes;
    }

    public string Dir { get; }

    public long MaxBytes { get; }

    public static AttachmentsOptions FromConfiguration(IConfiguration configuration)
    {
        var dir = configuration["Vorcall:AttachmentsDir"]?.Trim();
        return new AttachmentsOptions(
            string.IsNullOrEmpty(dir) ? DefaultDir : dir,
            ParseMaxBytes(configuration["Vorcall:AttachmentsMaxBytes"]));
    }

    // The four types the protocol accepts, and the extension each one is stored under.
    public static bool TryExtension(string contentType, out string ext)
    {
        ext = contentType switch
        {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => string.Empty,
        };

        return ext.Length > 0;
    }

    // The first bytes every accepted type must start with, checked while the body streams in so a
    // renamed file is refused rather than stored. Shared by both stores: the attachments and the
    // images accept exactly the same four types.
    public static bool MatchesMagic(string contentType, ReadOnlySpan<byte> header) => contentType switch
    {
        "image/png" => header.StartsWith(PngMagic),
        "image/jpeg" => header.StartsWith(JpegMagic),
        "image/gif" => header.StartsWith("GIF87a"u8) || header.StartsWith("GIF89a"u8),
        "image/webp" => header.StartsWith("RIFF"u8) && header[8..12].SequenceEqual("WEBP"u8),
        _ => false,
    };

    private static long ParseMaxBytes(string? configured)
    {
        if (string.IsNullOrWhiteSpace(configured))
        {
            return DefaultMaxBytes;
        }

        if (!long.TryParse(configured.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var maxBytes) || maxBytes < 1)
        {
            throw new InvalidOperationException("Invalid configuration 'Vorcall:AttachmentsMaxBytes' (expected a byte count of at least 1).");
        }

        return maxBytes;
    }
}
