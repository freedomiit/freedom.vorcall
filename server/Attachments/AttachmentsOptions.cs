using System.Globalization;

namespace Vorcall.Server.Attachments;

// Where uploaded files live and how many bytes of them the server will keep. The directory has a
// usable default; a quota that is present but unusable fails the boot like the other Vorcall
// options, because silently falling back to a default on a typo is how a disk fills up.
//
// An attachment may be any file of any type: nothing here sniffs one. The four Image* members are
// the separate, deliberately narrow rules of the image store — avatars, banners, the server icon
// and role icons — which still accept four types only, magic-checked, at 8 MiB.
public sealed record AttachmentsOptions
{
    public const string DefaultDir = "/attachments";

    // With 2 GiB allowed per file the quota is the only thing standing between uploads and a full
    // volume, so an operator is expected to set Vorcall:AttachmentsMaxBytes from the real size of
    // the disk; this default is merely a value a small deployment will not trip over.
    public const long DefaultMaxBytes = 200L << 30;

    // Per file, from PROTOCOL.md § Attachments: above this the upload is refused with 413. long
    // rather than int because 2 << 30 overflows a signed 32-bit int.
    public const long MaxFileBytes = 2L << 30;

    public const int MaxPerMessage = 4;

    // Every attachment is stored under its row id and this one extension: the type the client
    // declared is metadata and never part of a path.
    public const string StoredExtension = "bin";

    // Per image file: avatars, banners and icons keep the old cap.
    public const int ImageMaxFileBytes = 8 << 20;

    // Enough for the longest magic number the four accepted image types use (RIFF....WEBP).
    public const int ImageMagicLength = 12;

    // A soundpad clip: the VORCSND1 container of PROTOCOL.md § Sounds. 10 minutes of 96 kbit/s
    // stereo Opus is about 7.2 MB, so this leaves real headroom without letting an album through.
    public const int SoundMaxFileBytes = 16 << 20;

    // 30000 packets of 20 ms is the 10 minutes above; the packet cap is libopus's largest frame.
    public const int SoundMaxFrames = 30_000;

    public const int SoundMaxPacketBytes = 1275;

    public const int SoundHeaderBytes = 18;

    public const string SoundMediaType = "application/vnd.vorcall.sound";

    // A sticker: one of the four image types, magic-checked like an image, at a far smaller cap
    // since it is drawn at sticker size and never as a banner.
    public const int StickerMaxFileBytes = 1 << 20;

    // Complete rows only: an upload still in flight is not yet part of the library.
    public const int MaxStickers = 200;

    // An upload nothing linked to a message within this is swept, file and row together; the same
    // age makes an unreferenced image sweepable.
    public static readonly TimeSpan UnlinkedTtl = TimeSpan.FromHours(1);

    // An incomplete row is an upload that may still be streaming, and 2 GiB over a slow link can
    // outlive UnlinkedTtl several times over, so the sweeper gives it a day before taking it.
    public static readonly TimeSpan IncompleteTtl = TimeSpan.FromHours(24);

    // RFC 9110 § 5.6.2 token characters other than the alphanumerics, which are tested separately.
    private const string TokenPunctuation = "!#$%&'*+-.^_`|~";

    // The attachments.content_type column is varchar(128); a longer media type could not be stored
    // even if a client insisted on one.
    private const int MaxMediaTypeLength = 128;

    private static ReadOnlySpan<byte> PngMagic => [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

    private static ReadOnlySpan<byte> JpegMagic => [0xFF, 0xD8, 0xFF];

    public static ReadOnlySpan<byte> SoundMagic => "VORCSND1"u8;

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

    // An RFC 9110 type/subtype and nothing else: no parameters, both halves non-empty tokens. That
    // is all an attachment's declared type has to be, because nothing on the upload path interprets
    // it — it is stored, echoed back to clients and handed to the download as it arrived.
    public static bool IsValidMediaType(string value)
    {
        if (value.Length is 0 or > MaxMediaTypeLength)
        {
            return false;
        }

        // A second slash, a parameter's semicolon and whitespace are all non-token characters, so
        // the two halves being tokens is the whole grammar.
        var slash = value.IndexOf('/');
        if (slash <= 0 || slash == value.Length - 1)
        {
            return false;
        }

        return IsToken(value.AsSpan(0, slash)) && IsToken(value.AsSpan(slash + 1));
    }

    // The four image types the image store accepts, and the extension each one is stored under.
    // Attachments are not on this table: they take any type and one extension.
    public static bool TryImageExtension(string contentType, out string ext)
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

    // The first bytes every accepted image type must start with, checked while the body streams in
    // so a renamed file is refused rather than stored. The image store alone: an attachment may be
    // any file, so there is no magic number to hold it to.
    public static bool MatchesImageMagic(string contentType, ReadOnlySpan<byte> header) => contentType switch
    {
        "image/png" => header.StartsWith(PngMagic),
        "image/jpeg" => header.StartsWith(JpegMagic),
        "image/gif" => header.StartsWith("GIF87a"u8) || header.StartsWith("GIF89a"u8),
        "image/webp" => header.StartsWith("RIFF"u8) && header[8..12].SequenceEqual("WEBP"u8),
        _ => false,
    };

    private static bool IsToken(ReadOnlySpan<char> value)
    {
        foreach (var c in value)
        {
            if (!char.IsAsciiLetterOrDigit(c) && !TokenPunctuation.Contains(c))
            {
                return false;
            }
        }

        return true;
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
