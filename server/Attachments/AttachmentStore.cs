using System.Text;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;
using Vorcall.Server.Metrics;

namespace Vorcall.Server.Attachments;

// Attachment is set only when Status is Stored.
public sealed record StoreOutcome(StoreOutcome.Kind Status, Protocol.Attachment? Attachment)
{
    public enum Kind
    {
        Stored,
        TooLarge,
        QuotaExceeded,
        NotAnImage,
        Truncated,

        // The upload was fine and this server could not write it down.
        StorageFailed,
    }

    public static StoreOutcome Rejected(Kind kind) => new(kind, null);

    public static StoreOutcome Stored(Protocol.Attachment attachment) => new(Kind.Stored, attachment);
}

// The row and the file, always together: nothing here leaves a row without its bytes or bytes
// without their row. The file is named after the row id, so the name a client sent is metadata
// and never a path.
public sealed class AttachmentStore(
    AttachmentsOptions options,
    IDbContextFactory<AppDbContext> contexts,
    ServerMetrics metrics,
    ILogger<AttachmentStore> logger)
{
    public const int MaxFileNameLength = 128;

    // Enough for the longest magic number the protocol's four types use (RIFF....WEBP).
    private const int MagicLength = 12;

    private const int CopyBufferBytes = 64 * 1024;
    private const string PartSuffix = ".part";

    private static ReadOnlySpan<byte> PngMagic => [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

    private static ReadOnlySpan<byte> JpegMagic => [0xFF, 0xD8, 0xFF];

    public string PathFor(long id, string contentType)
    {
        if (!AttachmentsOptions.TryExtension(contentType, out var ext))
        {
            throw new ArgumentException($"Unsupported attachment content type '{contentType}'.", nameof(contentType));
        }

        var root = Path.TrimEndingDirectorySeparator(Path.GetFullPath(options.Dir));
        var path = Path.GetFullPath(Path.Combine(root, $"{id}.{ext}"));

        // Ids are numbers and the extension is one of four literals, so there is no traversal to
        // filter out; this is the assertion that keeps it that way.
        if (Path.GetDirectoryName(path) != root)
        {
            throw new InvalidOperationException("Attachment path escaped the attachments directory.");
        }

        return path;
    }

    public async Task<long> TotalBytesAsync()
    {
        await using var db = await contexts.CreateDbContextAsync();
        return await db.Attachments.SumAsync(a => (long?)a.Size) ?? 0;
    }

    public async Task<StoreOutcome> StoreAsync(
        long uploaderId,
        string roomId,
        string contentType,
        string? fileName,
        long declaredLength,
        Stream body,
        CancellationToken ct)
    {
        if (!AttachmentsOptions.TryExtension(contentType, out var ext))
        {
            return StoreOutcome.Rejected(StoreOutcome.Kind.NotAnImage);
        }

        if (declaredLength > AttachmentsOptions.MaxFileBytes)
        {
            return StoreOutcome.Rejected(StoreOutcome.Kind.TooLarge);
        }

        var storedBytes = await TotalBytesAsync();
        if (storedBytes + declaredLength > options.MaxBytes)
        {
            logger.LogWarning(
                "Attachment upload by {UserId} refused: {StoredBytes} stored plus {DeclaredBytes} declared exceeds the {QuotaBytes} quota",
                uploaderId,
                storedBytes,
                declaredLength,
                options.MaxBytes);
            return StoreOutcome.Rejected(StoreOutcome.Kind.QuotaExceeded);
        }

        await using var db = await contexts.CreateDbContextAsync();

        // The row is written first because the id names the file.
        var row = new Data.Attachment
        {
            RoomId = roomId,
            UploaderId = uploaderId,
            MessageId = null,
            FileName = SanitizeFileName(fileName, ext),
            ContentType = contentType,
            Size = declaredLength,
            CreatedAt = DateTime.UtcNow,
        };

        db.Attachments.Add(row);
        await db.SaveChangesAsync(ct);

        var path = PathFor(row.Id, contentType);
        var partPath = path + PartSuffix;
        StoreOutcome.Kind? failure = null;
        try
        {
            Directory.CreateDirectory(options.Dir);

            var header = new byte[MagicLength];
            var headerLength = 0;
            var written = 0L;

            // Written to a .part name and renamed at the end, so a file under the final name is
            // always a complete upload of the declared length.
            await using (var file = new FileStream(partPath, FileMode.Create, FileAccess.Write, FileShare.None))
            {
                var buffer = new byte[CopyBufferBytes];
                while (true)
                {
                    int read;
                    try
                    {
                        read = await body.ReadAsync(buffer, ct);
                    }
                    catch (IOException)
                    {
                        // Kestrel reports a client that dropped mid-body as an IOException
                        // (BadHttpRequestException, ConnectionResetException): a truncated upload,
                        // not this server failing to write it down.
                        logger.LogDebug("Attachment {AttachmentId} upload aborted", row.Id);
                        failure = StoreOutcome.Kind.Truncated;
                        break;
                    }

                    if (read == 0)
                    {
                        break;
                    }

                    written += read;
                    if (written > declaredLength)
                    {
                        failure = StoreOutcome.Kind.TooLarge;
                        break;
                    }

                    if (headerLength < header.Length)
                    {
                        var copied = Math.Min(header.Length - headerLength, read);
                        buffer.AsSpan(0, copied).CopyTo(header.AsSpan(headerLength));
                        headerLength += copied;
                        if (headerLength == header.Length && !MatchesMagic(contentType, header))
                        {
                            failure = StoreOutcome.Kind.NotAnImage;
                            break;
                        }
                    }

                    await file.WriteAsync(buffer.AsMemory(0, read), ct);
                }
            }

            // A body too short to carry a magic number is not one of the four types either.
            if (failure is null && headerLength < header.Length)
            {
                failure = StoreOutcome.Kind.NotAnImage;
            }

            if (failure is null && written != declaredLength)
            {
                failure = StoreOutcome.Kind.Truncated;
            }

            if (failure is null)
            {
                File.Move(partPath, path, overwrite: true);
            }
        }
        catch (OperationCanceledException)
        {
            logger.LogDebug("Attachment {AttachmentId} upload aborted", row.Id);
            failure = StoreOutcome.Kind.Truncated;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // The caller sent a perfectly good image and the disk refused it, so this is not a
            // rejected upload: it is this server failing, and the only line that says so.
            logger.LogError(ex, "Attachment {AttachmentId} could not be written to storage", row.Id);
            failure = StoreOutcome.Kind.StorageFailed;
        }

        if (failure is { } rejected)
        {
            TryDeleteFile(partPath);

            // Not ct: a cancelled upload still has to take its row with it.
            db.Attachments.Remove(row);
            await db.SaveChangesAsync(CancellationToken.None);
            return StoreOutcome.Rejected(rejected);
        }

        logger.LogDebug(
            "Attachment {AttachmentId} stored for {UserId} in {RoomId} ({SizeBytes} bytes)",
            row.Id,
            uploaderId,
            roomId,
            row.Size);
        metrics.CountUpload();
        return StoreOutcome.Stored(new Protocol.Attachment
        {
            Id = row.Id,
            FileName = row.FileName,
            ContentType = row.ContentType,
            Size = row.Size,
        });
    }

    // The entity rather than the protocol message: the download endpoint decides what a caller
    // may see from MessageId, UploaderId and RoomId.
    public async Task<Data.Attachment?> FindAsync(long id)
    {
        await using var db = await contexts.CreateDbContextAsync();
        return await db.Attachments.AsNoTracking().FirstOrDefaultAsync(a => a.Id == id);
    }

    // Best effort by design: the rows are already gone, and a file left behind is disk to
    // reclaim, not a correctness problem.
    public void DeleteFiles(IEnumerable<(long Id, string ContentType)> rows)
    {
        foreach (var (id, contentType) in rows)
        {
            if (!AttachmentsOptions.TryExtension(contentType, out _))
            {
                continue;
            }

            TryDeleteFile(PathFor(id, contentType));
        }
    }

    public async Task<int> SweepUnlinkedAsync(DateTime olderThan)
    {
        await using var db = await contexts.CreateDbContextAsync();
        var stale = await db.Attachments
            .AsNoTracking()
            .Where(a => a.MessageId == null && a.CreatedAt < olderThan)
            .Select(a => new { a.Id, a.ContentType })
            .ToListAsync();
        if (stale.Count == 0)
        {
            return 0;
        }

        var ids = stale.Select(a => a.Id).ToList();
        var deleted = await db.Attachments
            .Where(a => ids.Contains(a.Id) && a.MessageId == null)
            .ExecuteDeleteAsync();

        // Anything still there was linked to a message between the read and the delete, and its
        // file now belongs to that message.
        var survivors = await db.Attachments
            .AsNoTracking()
            .Where(a => ids.Contains(a.Id))
            .Select(a => a.Id)
            .ToListAsync();

        DeleteFiles(stale.Where(a => !survivors.Contains(a.Id)).Select(a => (a.Id, a.ContentType)));
        logger.LogDebug("Swept {Count} unlinked attachments older than {Cutoff}", deleted, olderThan);
        return deleted;
    }

    // Metadata only: the name is echoed to clients and never used to build a path.
    public static string SanitizeFileName(string? raw, string ext)
    {
        var fallback = $"image.{ext}";
        if (string.IsNullOrWhiteSpace(raw))
        {
            return fallback;
        }

        var builder = new StringBuilder(MaxFileNameLength);
        foreach (var rune in Path.GetFileName(raw.Trim()).EnumerateRunes())
        {
            // GetFileName only strips the platform's own separators, so a Windows-style path in
            // the header still arrives with backslashes in it.
            if (Rune.IsControl(rune) || rune.Value is '/' or '\\')
            {
                continue;
            }

            if (builder.Length + rune.Utf16SequenceLength > MaxFileNameLength)
            {
                break;
            }

            builder.Append(rune);
        }

        var name = builder.ToString().Trim();
        return name.Length == 0 ? fallback : name;
    }

    private static bool MatchesMagic(string contentType, ReadOnlySpan<byte> header) => contentType switch
    {
        "image/png" => header.StartsWith(PngMagic),
        "image/jpeg" => header.StartsWith(JpegMagic),
        "image/gif" => header.StartsWith("GIF87a"u8) || header.StartsWith("GIF89a"u8),
        "image/webp" => header.StartsWith("RIFF"u8) && header[8..12].SequenceEqual("WEBP"u8),
        _ => false,
    };

    private void TryDeleteFile(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or NotSupportedException or ArgumentException)
        {
            logger.LogDebug("Could not delete attachment file {Path} ({Reason})", path, ex.GetType().Name);
        }
    }
}
