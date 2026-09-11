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

    // An upload nothing linked to a message within this is swept, file and row together.
    public static readonly TimeSpan UnlinkedTtl = TimeSpan.FromHours(1);

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
